"""The compiled mode of `ultron autoreview`: one strong model writes a review *program*; the host executes it.

The fast and deep passes (autoreview_api.py, autoreview_deep.py) put a model in the loop of every lookup. Here the
expensive reasoning happens once, up front: a planner frame reads the whole change, the brief, the intent and the
rubric, decides what must be true for the change to be correct and safe, and writes down the checks that establish
it as a JSON program. The host then runs that program deterministically (code controls the flow) and calls a cheap
small model only at the decision points the program marks (`ask`), with the exact material each question needs.

1. Program. A list of steps with ids and `needs`. Ops: the deep pass's read-only lookups (`read`, `grep`, `list`,
   `definition`, `references`, `history`, `blame_range`, `pickaxe`; `grep` takes a line window and `count_only`),
   its sandboxed test executions (`run_tests`, `mutation_check`), `for_each` (a sub-program template over the items
   of a result), `ask` (one yes/no/unclear question to the small model, with a quote the host checks), `assert` (a
   predicate over a result, with the planner's `expect`) and `finding` (emitted when its condition holds, with its
   evidence named up front). `when` conditions on any step.
2. Validation. The program is checked against the language (ids, references, cycles, ops, predicates, bounds, and
   that every finding rests on an ask, a test or an exact-count presence check) before anything runs; a bad program
   gets one repair round with the planner; a program still bad means the review falls back to the `both` mode.
3. Execution. An assert is true, false or *unknown*: unknown when the step it reads could not run or was skipped,
   when a count was cut at its cap (a capped count is a lower bound: predicates the bound settles are decided, the
   rest are unknown), or when it looks for text in a truncated result. A finding whose condition is unknown, or
   whose deterministic check contradicts the planner's expectation, is not dropped: the host puts the finding, the
   check and the raw results to the small model (`RESOLVE_TASK`) and emits it as model-judged when the answer is
   yes. Test steps for a runner the automatic run found unavailable are `could_not_run` at once. Independent steps
   run concurrently (asks under the frame concurrency); every step's input, output, duration and tokens are
   recorded. Lookups and tests go through the deep pass's validation and limits.
4. Findings. A finding whose condition roots in deterministic results that came out as expected is confirmed by
   that evidence and skips the verifier; critical and high stand only when a run showed the failure, else such a
   finding is at most medium. A finding whose condition or evidence rests on an `ask` (planned or host-generated)
   goes through the existing verifier frame. The asserts that held as expected become the review's assurance.

Models have no tools, nothing of the reviewed repository runs outside the sandbox, and the planner can request
nothing but the fixed read-only lookups and the sandboxed tests. Everything a frame sees is data.
"""
from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import tempfile
from typing import Any, Callable

from infer_api import FrameError, Incomplete
from review_api import FileDiff, _clip, _text, normalize_category, source_window
import autoreview_deep as deep
import autoreview_tests as testing
from review_prompts import (ASK_TASK, CHECK_CATALOGUE, COMPILED_REPAIR, RESOLVE_TASK, RV_API, TRIGGERED_SHAPES,
                            cell_planner_task, compiled_planner_task)

LOOKUPS = ("read", "grep", "list", "definition", "references", "history", "blame_range", "pickaxe")
TEST_OPS = ("run_tests", "mutation_check")
#: Results with items a `for_each` may iterate.
ITERABLE = ("grep", "references", "list", "history", "blame_range", "pickaxe")
OPS = (*LOOKUPS, *TEST_OPS, "for_each", "ask", "assert", "finding")
LEVELS = ("critical", "high", "medium", "low", "nit")
OLD_LEVELS = {"blocker": "critical", "major": "high", "minor": "low"}
LEVEL_TO_OLD = {"critical": "blocker", "high": "major", "medium": "minor", "low": "minor", "nit": "nit"}
#: Steps as written, steps after `for_each` expansion, asks, findings, items per `for_each`, sub-steps per template.
MAX_PROGRAM_STEPS = 80
MAX_STEPS = 120
#: A diff with more changed lines than this gets the larger step limits.
LARGE_DIFF_LINES = 100
LARGE_PROGRAM_STEPS = 120
LARGE_STEPS = 200
MAX_ASKS = 40
MAX_FINDINGS = 12
MAX_FOR_EACH_ITEMS = 20
DEFAULT_FOR_EACH_ITEMS = 8
MAX_TEMPLATE_STEPS = 8
MAX_QUESTION_CHARS = 1_500
ASK_CONTEXT_CHARS = 12_000
#: A result's text is kept up to this; a `contains` over a cut text that finds nothing is unknown.
RESULT_CHARS = 24_000
RECORD_CHARS = 1_200
EVIDENCE_CHARS = 1_200
MAX_HOLDS = 3
#: The retrieved-context block the planner gets before it plans: references, tests and sibling families of the
#: changed names, mechanically looked up by the host.
RETRIEVAL_CHARS = 24_000
RETRIEVAL_HITS = 12
#: The planner as a REPL: cells the strong model writes, each run in a sandboxed child whose only interface to the
#: repository is `rv`.
PLAN_STYLES = ("cell", "frame")
DEFAULT_PLAN_STYLE = "cell"
DEFAULT_PLAN_CELLS = 8
MAX_PLAN_CELLS = 12
#: The planner's thinking level when it plans in cells (many short frames); the one-frame planner keeps "high".
DEFAULT_CELL_THINKING = "medium"
#: Cells sent back in full; earlier cells are one summary line each (the step records stay host-side).
CELL_TRANSCRIPT_FULL = 2
CELL_TIMEOUT_S = 120
CELL_OUTPUT_CHARS = 8_000
CELL_TRANSCRIPT_CHARS = 40_000
CELL_VALUE_CHARS = 4_000
CELL_VALUE_ITEMS = 20
CAPPED = "(more not shown)"
_ID = re.compile(r"^[A-Za-z_][A-Za-z0-9_-]{0,39}$")
_PLACEHOLDER = re.compile(r"\{\{\s*([A-Za-z_][A-Za-z0-9_\-]*(?:\[\d+\])?(?:\.[A-Za-z_][A-Za-z0-9_\-]*)*)\s*\}\}")
_COUNT = re.compile(r"^count\s*(==|!=|>=|<=|>|<)\s*(\d+)$")
_WORD = re.compile(r"^(status|answer)\s*(==|!=)\s*([a-z_]+)$")
_CONTAINS = re.compile(r"^(not\s+)?contains\s+(.+)$", re.S)
_HIT = re.compile(r"^(.+?):(\d+): ?(.*)$")
_TITLE_COUNT = re.compile(r"-> (\d+) (?:matches|commits)|\((\d+) entries\)")
STATUSES = ("passed", "failed", "could_not_run")
ANSWERS = ("yes", "no", "unclear")

PROGRAM_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "summary": {"type": "string"},
        "uncovered": {"type": "array", "items": {"type": "string"}},
        "steps": {
            "type": "array",
            "maxItems": LARGE_PROGRAM_STEPS,
            "items": {"type": "object", "properties": {"id": {"type": "string"}, "op": {"enum": list(OPS)}},
                      "required": ["id", "op"]},
        },
    },
    "required": ["steps"],
}

ASK_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {"answer": {"enum": list(ANSWERS)}, "quote": {"type": "string"}, "why": {"type": "string"}},
    "required": ["answer", "quote"],
}


# --- Validation ------------------------------------------------------------------------------------------------


def _flat(text: str) -> str:
    return " ".join(text.split())


def _parse_predicate(text: Any) -> tuple[str, ...] | None:
    """A predicate as a tuple, or None: ("count", op, n), ("status"|"answer", op, word), ("contains", needle, negated)."""
    if not isinstance(text, str) or not text.strip():
        return None
    text = text.strip()
    match = _COUNT.match(text)
    if match:
        return ("count", match.group(1), match.group(2))
    match = _WORD.match(text)
    if match:
        allowed = STATUSES if match.group(1) == "status" else ANSWERS
        return (match.group(1), match.group(2), match.group(3)) if match.group(3) in allowed else None
    match = _CONTAINS.match(text)
    if match:
        needle = match.group(2).strip()
        if len(needle) >= 2 and needle[0] == needle[-1] and needle[0] in "\"'":
            needle = needle[1:-1]
        return ("contains", needle, "not" if match.group(1) else "") if needle else None
    return None


def _when(raw: Any) -> tuple[dict[str, Any] | None, str | None]:
    if raw is None:
        return None, None
    if isinstance(raw, str):
        raw = {"step": raw}
    if not isinstance(raw, dict) or not isinstance(raw.get("step"), str):
        return None, "when must be {\"step\": <id>} or {\"step\": <id>, \"not\": true}"
    return {"step": raw["step"], "not": raw.get("not") is True}, None


def _ids(raw: Any) -> list[str]:
    return [item for item in raw if isinstance(item, str)] if isinstance(raw, list) else []


def limits_for(changed_lines: int) -> tuple[int, int]:
    """(steps as written, steps after expansion) a program may have for a diff of `changed_lines` lines."""
    if changed_lines > LARGE_DIFF_LINES:
        return LARGE_PROGRAM_STEPS, LARGE_STEPS
    return MAX_PROGRAM_STEPS, MAX_STEPS


class Program:
    """A validated program: top-level steps in order, each normalized, plus the template steps of its for_each;
    the coverage items it declares it could not check, with the planner's reasons."""

    def __init__(self, steps: list[dict[str, Any]], summary: str, uncovered: list[str] | None = None) -> None:
        self.steps = steps
        self.summary = summary
        self.uncovered = list(uncovered or [])

    def as_json(self) -> dict[str, Any]:
        out: dict[str, Any] = {"summary": self.summary, "steps": [_public_step(step) for step in self.steps]}
        if self.uncovered:
            out["uncovered"] = self.uncovered
        return out


def _public_step(step: dict[str, Any]) -> dict[str, Any]:
    out = {key: value for key, value in step.items() if key not in ("predicate_parsed",) and value not in (None, [], {})}
    if "steps" in out:
        out["steps"] = [_public_step(item) for item in out["steps"]]
    return out


def _normalize_step(raw: Any, errors: list[str], *, inside: str | None = None) -> dict[str, Any] | None:
    """One step's shape checked and normalized; errors are appended with the step's id."""
    if not isinstance(raw, dict):
        errors.append("a step is not an object")
        return None
    sid = raw.get("id")
    label = f"step {sid!r}" if isinstance(sid, str) else "a step without id"
    if not isinstance(sid, str) or not _ID.match(sid):
        errors.append(f"{label}: id must match {_ID.pattern}")
        return None
    op = raw.get("op")
    if op not in OPS:
        errors.append(f"{label}: unknown op {op!r}; use one of {', '.join(OPS)}")
        return None
    step: dict[str, Any] = {"id": sid, "op": op, "needs": _ids(raw.get("needs")), "covers": _ids(raw.get("covers"))}
    when, problem = _when(raw.get("when"))
    if problem:
        errors.append(f"{label}: {problem}")
    step["when"] = when
    if op in LOOKUPS or op in TEST_OPS:
        args = raw.get("args")
        if not isinstance(args, dict):
            errors.append(f"{label}: {op} takes an args object")
            return None
        needed = {"read": ("path",), "grep": ("pattern",), "list": ("dir",), "definition": ("symbol",),
                  "references": ("symbol",), "history": ("path",), "blame_range": ("path",), "pickaxe": ("string",),
                  "run_tests": ("paths",), "mutation_check": ("path", "line", "replacement", "tests")}[op]
        for key in needed:
            if key not in args:
                errors.append(f"{label}: {op} needs args.{key}")
        step["args"] = args
    elif op == "for_each":
        if inside is not None:
            errors.append(f"{label}: for_each cannot be nested")
            return None
        if not isinstance(raw.get("over"), str):
            errors.append(f"{label}: for_each needs over (the id of a grep, references, list or history step)")
        step["over"] = raw.get("over")
        try:
            step["max_items"] = min(MAX_FOR_EACH_ITEMS, max(1, int(raw.get("max_items") or DEFAULT_FOR_EACH_ITEMS)))
        except (TypeError, ValueError):
            step["max_items"] = DEFAULT_FOR_EACH_ITEMS
        template = raw.get("steps")
        if not isinstance(template, list) or not 1 <= len(template) <= MAX_TEMPLATE_STEPS:
            errors.append(f"{label}: for_each takes 1 to {MAX_TEMPLATE_STEPS} sub-steps")
            template = []
        step["steps"] = [item for item in (_normalize_step(sub, errors, inside=sid) for sub in template) if item]
    elif op == "ask":
        question = raw.get("question")
        if not isinstance(question, str) or not question.strip():
            errors.append(f"{label}: ask needs a question")
        elif len(question) > MAX_QUESTION_CHARS:
            errors.append(f"{label}: the question is longer than {MAX_QUESTION_CHARS} characters")
        step["question"] = question if isinstance(question, str) else ""
        step["context"] = _ids(raw.get("context"))
        if not step["context"]:
            errors.append(f"{label}: ask needs context: the ids of the steps whose results answer it")
    elif op == "assert":
        forms = [key for key in ("step", "all", "any") if raw.get(key) is not None]
        if len(forms) != 1:
            errors.append(f"{label}: assert takes exactly one of step (with predicate), all, any")
        if raw.get("step") is not None:
            if not isinstance(raw.get("step"), str):
                errors.append(f"{label}: assert.step must be an id")
            parsed = _parse_predicate(raw.get("predicate"))
            if parsed is None:
                errors.append(f"{label}: predicate {raw.get('predicate')!r} is not one of count ==|!=|>=|<=|>|< N, "
                              "status == passed|failed|could_not_run, answer == yes|no|unclear, contains <text>, "
                              "not contains <text>")
            step["step"] = raw.get("step")
            step["predicate"] = raw.get("predicate") if isinstance(raw.get("predicate"), str) else ""
            step["predicate_parsed"] = parsed
        for key in ("all", "any"):
            if raw.get(key) is not None:
                step[key] = _ids(raw.get(key))
                if not step[key]:
                    errors.append(f"{label}: assert.{key} must list ids")
        if not isinstance(raw.get("expect"), bool):
            errors.append(f"{label}: assert needs expect: true or false, the value you believe it will have")
        step["expect"] = raw.get("expect") if isinstance(raw.get("expect"), bool) else None
        step["holds"] = raw.get("holds") if isinstance(raw.get("holds"), str) else ""
    elif op == "finding":
        when, problem = _when(raw.get("when"))
        if when is None:
            errors.append(f"{label}: a finding needs when, naming an assert or an ask")
        step["when"] = when
        for key in ("file", "claim"):
            if not isinstance(raw.get(key), str) or not raw[key].strip():
                errors.append(f"{label}: finding needs {key}")
        line = raw.get("line")
        if not (isinstance(line, int) and not isinstance(line, bool)) and not (
                isinstance(line, str) and (line.strip().isdigit() or _PLACEHOLDER.search(line))):
            errors.append(f"{label}: finding needs line (a number or a placeholder)")
        level = str(raw.get("level", "")).strip().lower()
        if level not in LEVELS and level not in OLD_LEVELS:
            errors.append(f"{label}: level must be one of {', '.join(LEVELS)}")
        step["evidence"] = _ids(raw.get("evidence"))
        citations = raw.get("citations")
        step["citations"] = [item for item in citations if isinstance(item, dict)] if isinstance(citations, list) else []
        if not step["evidence"] and not step["citations"]:
            errors.append(f"{label}: finding needs evidence (step ids) or citations")
        for key in ("file", "line", "level", "category", "claim", "why", "fix", "scenario", "unpinned", "consequence"):
            step[key] = raw.get(key)
    return step


def _references(step: dict[str, Any]) -> list[str]:
    """Every step id a step depends on (its needs and the ids its op reads)."""
    refs = list(step.get("needs") or [])
    if step.get("when"):
        refs.append(step["when"]["step"])
    for key in ("over", "step"):
        if isinstance(step.get(key), str):
            refs.append(step[key])
    for key in ("context", "all", "any", "evidence"):
        refs += step.get(key) or []
    return list(dict.fromkeys(refs))


def _assert_leaves(sid: str, by_id: dict[str, dict[str, Any]], seen: set[str] | None = None) -> list[dict[str, Any]]:
    """The non-assert steps an assert (or ask) ultimately reads, through all/any."""
    seen = seen if seen is not None else set()
    if sid in seen or sid not in by_id:
        return []
    seen.add(sid)
    step = by_id[sid]
    if step["op"] != "assert":
        return [step]
    out: list[dict[str, Any]] = []
    for ref in ([step["step"]] if isinstance(step.get("step"), str) else []) + (step.get("all") or []) + (step.get("any") or []):
        out += _assert_leaves(ref, by_id, seen)
    return out


def grounded(step: dict[str, Any], by_id: dict[str, dict[str, Any]]) -> bool:
    """Whether a finding rests on something that can establish it: an ask (a semantic judgement), a test run, or
    an exact-count presence check (a count_only grep). A capped grep or a read alone establishes nothing."""
    when = step.get("when")
    if when is None or when["step"] not in by_id:
        return True  # reported elsewhere
    leaves = _assert_leaves(when["step"], by_id)
    if any(leaf["op"] == "ask" for leaf in leaves):
        return True
    if any(by_id.get(ref, {}).get("op") in TEST_OPS + ("ask",) for ref in step.get("evidence") or []):
        return True
    return bool(leaves) and all(leaf["op"] in TEST_OPS or (leaf["op"] == "grep" and (leaf.get("args") or {}).get("count_only") is True)
                                for leaf in leaves)


_EFFECT = re.compile(r"execute\(|\bDELETE\b|\bINSERT\b|\bUPDATE\b|\bDROP\b|\brm\b|\bmv\b|unlink|\.write\(|commit\(|\bsend|"
                     r"requests\.|fetch\(|subprocess|os\.system|shutil\.", re.I)
_GUARD = re.compile(r"validate|check|assert|verify|sanitiz|raise ValueError|throw new", re.I)
_ERROR_PATH = re.compile(r"\b(except|catch|raise|throw|finally|rollback|on_error|errexit)\b|set -e", re.I)
_RETRY = re.compile(r"\b(retry|retries|attempt|timeout|poll|backoff|sleep)\b", re.I)
_WORKFLOW = re.compile(r"(^|/)\.github/workflows/|(^|/)\.gitlab-ci|(^|/)ci/|Jenkinsfile|\.circleci/", re.I)
_MANIFEST = re.compile(r"(^|/)(k8s|helm|charts|deploy|infra|terraform|manifests?)/|\.tf$|docker-compose|(^|/)Dockerfile|"
                       r"\.(ya?ml)$", re.I)


def shape_items(brief: Any, files: list[FileDiff]) -> list[dict[str, str]]:
    """The catalogue shapes whose trigger the map detects in this change, as coverage items (`T<n>`): the planner
    must run that shape's check or declare it uncovered."""
    extracted = getattr(brief, "extracted", {}) or {}
    reviewable = [item for item in files if not deep.skip_reason(item)]
    code = [item for item in reviewable if deep.file_kind(item.path) == "code"]
    tests = [item for item in reviewable if deep.file_kind(item.path) == "test"]
    added = "\n".join(line.text for item in reviewable for hunk in item.hunks for line in hunk.lines if line.kind == "+")
    removed = "\n".join(line.text for item in reviewable for hunk in item.hunks for line in hunk.lines if line.kind == "-")
    paths = [item.path for item in files]
    triggered: dict[str, str] = {}
    if extracted.get("fields") or extracted.get("constants") or re.search(r"^\s*['\"][\w.-]+['\"],?\s*$", added, re.M):
        triggered["registry-member"] = ", ".join(f"`{name}`" for name in (extracted.get("fields") or extracted.get("constants") or [])[:4]) or "a new list member"
    if code:
        triggered["unpinned-behaviour"] = ", ".join(item.path for item in code[:3])
    if tests:
        triggered["test-asserts-behaviour"] = ", ".join(item.path for item in tests[:3])
    if extracted.get("env") or extracted.get("flags") or extracted.get("config"):
        triggered["env-in-deploy"] = ", ".join(f"`{name}`" for name in (extracted.get("env") or []) + (extracted.get("flags") or []) + (extracted.get("config") or [])[:4])
    if any(_WORKFLOW.search(path) for path in paths):
        triggered["workflow-siblings"] = ", ".join(path for path in paths if _WORKFLOW.search(path))[:200]
    if any(_MANIFEST.search(path) and not _WORKFLOW.search(path) for path in paths):
        triggered["manifest-reference"] = ", ".join(path for path in paths if _MANIFEST.search(path) and not _WORKFLOW.search(path))[:200]
    if code and deep._INPUT_HINT.search(added):
        triggered["input-defeats-guard"] = "the new guard, pattern, limit or parser in the diff"
    if code and _GUARD.search(added) and _EFFECT.search(added + "\n" + removed):
        triggered["guard-after-effect"] = "the new check beside a side-effecting operation"
    if _ERROR_PATH.search(added) or _ERROR_PATH.search(removed):
        triggered["error-path"] = "the changed except/catch/raise/rollback lines"
    if extracted.get("claims") or any(deep.file_kind(item.path) == "doc" for item in reviewable):
        triggered["comment-vs-code"] = "; ".join(_clip(text, 80) for _p, _l, text in (extracted.get("claims") or [])[:3]) or "the changed documents"
    if "same name elsewhere:" in (getattr(brief, "text", "") or ""):
        triggered["sibling-implementation"] = "the twin file(s) the brief names"
    if code and _RETRY.search(added):
        triggered["failure-retry"] = "the new retry, timeout or polling code"
    by_key = {shape["key"]: shape for shape in CHECK_CATALOGUE}
    items: list[dict[str, str]] = []
    for number, key in enumerate([key for key in TRIGGERED_SHAPES if key in triggered], 1):
        shape = by_key[key]
        items.append({"id": f"T{number}", "kind": "shape", "name": key,
                      "text": f"catalogue shape {key} ({triggered[key]}): when {shape['when']}; how: {shape['how']}"})
    return items


def coverage_items(brief: Any, files: list[FileDiff] | None = None) -> list[dict[str, str]]:
    """What a program must cover, from the map: the references of each changed signature or exported name (S),
    the siblings and consumers of each new config key, field, flag or environment variable (K), and each claim of
    the change (C). Each item has an id the planner names in a step's `covers` or in the program's `uncovered`."""
    items: list[dict[str, str]] = []
    required = getattr(brief, "required", {}) or {}
    extracted = getattr(brief, "extracted", {}) or {}
    for number, name in enumerate(required.get("siblings") or [], 1):
        items.append({"id": f"S{number}", "kind": "symbol", "name": name,
                      "text": f"the references of `{name}` (changed signature or exported name): every caller still fits"})
    keys = list(dict.fromkeys(list(required.get("claims") or []) + list(extracted.get("fields") or [])))[:8]
    for number, name in enumerate(keys, 1):
        items.append({"id": f"K{number}", "kind": "key", "name": name,
                      "text": f"the siblings and consumers of the new key, field, flag or variable `{name}`: the family it "
                              "joins (registry lists, sibling declarations) and every reader"})
    for number, (source, text) in enumerate(list(getattr(brief, "claim_list", []) or [])[:8], 1):
        items.append({"id": f"C{number}", "kind": "claim", "name": "", "text": f"the claim [{source}] {text}"})
    if files is not None:
        items += shape_items(brief, files)
    return items


def _step_texts(step: dict[str, Any]) -> str:
    """The text of a step a coverage name may appear in: its args, question, predicate, holds, claim and why."""
    parts = [json.dumps(step.get("args") or {}), step.get("question") or "", step.get("predicate") or "",
             step.get("holds") or "", str(step.get("claim") or ""), str(step.get("why") or "")]
    return "\n".join(part for part in parts if isinstance(part, str))


def uncovered_items(coverage: list[dict[str, str]], steps: list[dict[str, Any]], declared: list[str]) -> list[dict[str, str]]:
    """The coverage items no step covers and the planner did not declare uncovered. A step covers an item it names
    in `covers`; a symbol or key is also covered by a step whose text names it."""
    declared_ids = {item.split(":", 1)[0].strip() for item in declared}
    named: set[str] = set()
    texts: list[str] = []
    for step in steps:
        named.update(step.get("covers") or [])
        texts.append(_step_texts(step))
    text = "\n".join(texts)
    out = []
    for item in coverage:
        if item["id"] in declared_ids or item["id"] in named:
            continue
        if item["kind"] in ("symbol", "key") and item["name"] and re.search(r"(?<![A-Za-z0-9_])" + re.escape(item["name"]) + r"(?![A-Za-z0-9_])", text):
            continue
        out.append(item)
    return out


def _check_step(step: dict[str, Any], by_id: dict[str, dict[str, Any]], scope: set[str], errors: list[str]) -> None:
    """The reference rules of one step against the steps it may name."""
    for ref in _references(step):
        if ref not in by_id:
            errors.append(f"step {step['id']!r}: names unknown step {ref!r}")
        elif ref not in scope:
            errors.append(f"step {step['id']!r}: cannot name {ref!r}, a sub-step of another for_each")
    if step["op"] == "ask":
        for ref in step["context"]:
            if ref in by_id and by_id[ref]["op"] not in LOOKUPS + TEST_OPS:
                errors.append(f"step {step['id']!r}: context must name lookups or test steps, not {by_id[ref]['op']!r} {ref!r}")
    if step["op"] == "assert":
        for ref in [step.get("step")] if isinstance(step.get("step"), str) else []:
            if ref in by_id and by_id[ref]["op"] in ("assert", "finding", "for_each"):
                errors.append(f"step {step['id']!r}: a predicate applies to a lookup, test or ask, not to {by_id[ref]['op']!r} {ref!r}")
        for ref in (step.get("all") or []) + (step.get("any") or []):
            if ref in by_id and by_id[ref]["op"] not in ("assert", "ask"):
                errors.append(f"step {step['id']!r}: all/any list asserts and asks, not {by_id[ref]['op']!r} {ref!r}")
    if step.get("when") and step["when"]["step"] in by_id and by_id[step["when"]["step"]]["op"] not in ("assert", "ask"):
        errors.append(f"step {step['id']!r}: when must name an assert or an ask, not {by_id[step['when']['step']]['op']!r}")
    if step["op"] == "for_each" and step.get("over") in by_id and by_id[step["over"]]["op"] not in ITERABLE:
        errors.append(f"step {step['id']!r}: for_each iterates a grep, references, list or history result, not {by_id[step['over']]['op']!r}")
    if step["op"] == "finding":
        for ref in step["evidence"]:
            if ref in by_id and by_id[ref]["op"] not in LOOKUPS + TEST_OPS + ("ask",):
                errors.append(f"step {step['id']!r}: evidence names lookups, tests or asks, not {by_id[ref]['op']!r} {ref!r}")
        if not grounded(step, by_id):
            errors.append(f"step {step['id']!r}: a finding must rest on an ask (for code semantics), a test run, or an "
                          "exact-count presence check (a grep with \"count_only\": true); a capped grep count or a "
                          "read alone does not establish it")


def validate(raw: Any, coverage: list[dict[str, str]] | None = None,
             max_planned: int = MAX_PROGRAM_STEPS) -> tuple[Program | None, list[str]]:
    """The program `raw` (the planner's reply) checked against the language and, with `coverage`, against what the
    map says it must cover: (program, []) or (None, errors)."""
    errors: list[str] = []
    if not isinstance(raw, dict) or not isinstance(raw.get("steps"), list):
        return None, ["the program must be an object with a steps array"]
    if not raw["steps"]:
        return None, ["the program has no steps"]
    if len(raw["steps"]) > max_planned:
        return None, [f"the program has {len(raw['steps'])} steps; at most {max_planned} as written"]
    declared = [_text(item, 200) for item in raw.get("uncovered") or [] if isinstance(item, str)] if isinstance(raw.get("uncovered"), list) else []
    steps = [item for item in (_normalize_step(item, errors) for item in raw["steps"]) if item]
    by_id: dict[str, dict[str, Any]] = {}
    for step in steps:
        if step["id"] in by_id:
            errors.append(f"step {step['id']!r}: duplicate id")
        by_id[step["id"]] = step
        for sub in step.get("steps") or []:
            if sub["id"] in by_id:
                errors.append(f"step {sub['id']!r}: duplicate id (sub-step ids must be unique in the program too)")
            by_id[sub["id"]] = sub
    top = {step["id"] for step in steps}
    owner = {sub["id"]: step["id"] for step in steps for sub in step.get("steps") or []}

    for step in steps:
        _check_step(step, by_id, top, errors)
        for sub in step.get("steps") or []:
            _check_step(sub, by_id, top | {other["id"] for other in step["steps"]}, errors)
            if sub["op"] == "for_each":
                errors.append(f"step {sub['id']!r}: for_each cannot be nested")
    # No cycles: every reference must resolve to an earlier step in a topological order. A for_each finishes after
    # its sub-steps, so it depends on them.
    graph = {sid: [ref for ref in _references(step) if ref in by_id] for sid, step in by_id.items()}
    for sid, parent in owner.items():
        graph[parent].append(sid)
    state: dict[str, int] = {}

    def visit(node: str, path: list[str]) -> None:
        if state.get(node) == 2:
            return
        if state.get(node) == 1:
            errors.append("the dependencies form a cycle: " + " -> ".join(path[path.index(node):] + [node]))
            return
        state[node] = 1
        for ref in graph.get(node, []):
            visit(ref, path + [node])
        state[node] = 2

    for sid in by_id:
        visit(sid, [])
    asks = sum(1 for step in by_id.values() if step["op"] == "ask")
    if asks > MAX_ASKS:
        errors.append(f"{asks} ask steps; at most {MAX_ASKS}")
    findings = sum(1 for step in by_id.values() if step["op"] == "finding")
    if findings > MAX_FINDINGS:
        errors.append(f"{findings} finding steps; at most {MAX_FINDINGS}")
    if coverage:
        all_steps = [step for step in steps] + [sub for step in steps for sub in step.get("steps") or []]
        for item in uncovered_items(coverage, all_steps, declared):
            errors.append(f"coverage: {item['id']} ({item['text']}) has no check: add a step that checks it and name "
                          f"{item['id']} in its \"covers\", or list it in the program's \"uncovered\" with why")
        known = {item["id"] for item in coverage}
        for item in declared:
            if item.split(":", 1)[0].strip() not in known:
                errors.append(f"uncovered names {item.split(':', 1)[0].strip()!r}, which is not a coverage item")
    if errors:
        return None, list(dict.fromkeys(errors))[:40]
    return Program(steps, _text(raw.get("summary"), 300), declared), []


# --- Interpretation --------------------------------------------------------------------------------------------


def _count_of(op: str, title: str, body: str) -> int | None:
    match = _TITLE_COUNT.search(title)
    if match:
        return int(match.group(1) or match.group(2))
    if op == "read":
        return len(body.splitlines())
    return None


def _items_of(op: str, body: str) -> list[Any]:
    if op in ("grep", "references", "definition"):
        items = []
        for line in body.splitlines():
            match = _HIT.match(line)
            if match:
                items.append({"path": match.group(1), "line": int(match.group(2)), "text": match.group(3)})
        return items
    if op in ("list", "history", "blame_range", "pickaxe"):
        return [line for line in body.splitlines() if line.strip() and not line.startswith("...")
                and line not in ("0 matches", "no commits found", "no commit added or removed it",
                                 "no history available for these lines")]
    return []


def _render(value: Any) -> str:
    if isinstance(value, dict):
        if {"path", "line"} <= set(value):
            return f"{value['path']}:{value['line']}"
        return json.dumps(value)
    return "" if value is None else str(value)


def _count_predicate(count: int, capped: bool, op: str, n: int) -> tuple[bool | None, str]:
    """A count predicate over an exact count, or over a lower bound (a count cut at its cap): the predicates the
    bound settles are decided, the rest are unknown."""
    if not capped:
        value = {"==": count == n, "!=": count != n, ">=": count >= n, "<=": count <= n, ">": count > n, "<": count < n}[op]
        return value, f"count is {count}"
    detail = f"count is at least {count} (cut at its cap)"
    settled: bool | None = None
    if op == "==":
        settled = False if n < count else None
    elif op == "!=":
        settled = True if n < count else None
    elif op == ">=":
        settled = True if count >= n else None
    elif op == ">":
        settled = True if count > n else None
    elif op == "<":
        settled = False if count >= n else None
    elif op == "<=":
        settled = False if count > n else None
    return settled, detail


class Interpreter:
    """Runs one validated program against the repository at the reviewed commit."""

    def __init__(self, program: Program, repo: deep.Repo, *, frames: Any, session: Any, diff_lines: dict[str, set[int]],
                 ask_model: str | None, ask_thinking: str | None, cutoff: float | None, clock: Callable[[], float],
                 cap: Callable[..., str], to_level: Callable[[Any], str | None] | None = None,
                 enrich: Callable[[Any, dict[str, Any]], None] | None = None,
                 generic: Callable[[dict[str, Any]], str | None] | None = None,
                 unavailable: set[tuple[str, str]] | None = None, max_steps: int = MAX_STEPS) -> None:
        self.program = program
        self.max_steps = max_steps
        self.max_planned = LARGE_PROGRAM_STEPS if max_steps > MAX_STEPS else MAX_PROGRAM_STEPS
        self.repo = repo
        self.frames = frames
        self.session = session
        self.diff_lines = diff_lines
        self.ask_model = ask_model
        self.ask_thinking = ask_thinking
        self.cutoff = cutoff
        self.clock = clock
        self.cap = cap
        self.to_level = to_level
        self.enrich = enrich
        self.generic = generic
        #: (runner, directory) pairs the automatic run found unavailable: their test steps are could_not_run at once.
        self.unavailable: set[tuple[str, str]] = set(unavailable or ())
        #: Results by step id, in finishing order.
        self.results: dict[str, dict[str, Any]] = {}
        self.records: list[dict[str, Any]] = []
        self.findings: list[dict[str, Any]] = []
        self.dropped: list[str] = []
        self.generic_dropped: list[str] = []
        self.refuted = 0
        self.truncated: list[str] = []
        self.steps_by_id: dict[str, dict[str, Any]] = {}
        self.expanded = 0
        self.asks = 0
        self.auto_asks = 0
        self.resolved = 0
        #: Finding steps that emitted nothing, by why.
        self.not_emitted = {"gateFalse": 0, "undecided": 0, "askedNo": 0, "askedUnclear": 0}
        self.test_runs_before = len(session.records) if session is not None else 0

    # -- placeholders --

    def _value(self, ref: str, scope: dict[str, Any]) -> str | None:
        name, _, field = ref.partition(".")
        if name in scope:
            value = scope[name]
            if field:
                return _render(value.get(field)) if isinstance(value, dict) else None
            return _render(value)
        result = self.results.get(name)
        if result is None:
            return None
        if not field or field == "text":
            return result.get("summary") if not field else result.get("text", "")
        if field in ("count", "status", "answer", "quote", "why", "title", "value"):
            value = result.get("outcome") if field == "status" and result["op"] in TEST_OPS else result.get(field)
            return _render(value)
        return None

    def fill(self, text: Any, scope: dict[str, Any] | None = None) -> Any:
        """`text` with every {{placeholder}} replaced by the named result or item field."""
        if isinstance(text, dict):
            return {key: self.fill(value, scope) for key, value in text.items()}
        if isinstance(text, list):
            return [self.fill(value, scope) for value in text]
        if not isinstance(text, str):
            return text

        def replace(match: re.Match[str]) -> str:
            value = self._value(match.group(1), scope or {})
            return match.group(0) if value is None else value

        return _PLACEHOLDER.sub(replace, text)

    # -- truth --

    def truth(self, sid: str) -> bool | None:
        """Whether an assert or ask step holds: True, False, or None when it is unknown (the assert could not be
        decided, the ask was answered unclear, or the step did not run). An undecided question decides nothing,
        negated or not."""
        result = self.results.get(sid)
        if result is None or result["status"] != "ok":
            return None
        if result["op"] == "assert":
            return result.get("value")
        if result["op"] == "ask":
            return None if result.get("answer") == "unclear" else result.get("answer") == "yes"
        return None

    def _condition(self, when: dict[str, Any] | None) -> bool | None:
        if when is None:
            return True
        value = self.truth(when["step"])
        return None if value is None else (not value if when["not"] else value)

    def roots_in_ask(self, sid: str, seen: set[str] | None = None) -> bool:
        """Whether a step's truth depends, transitively, on a model's answer."""
        seen = seen or set()
        if sid in seen:
            return False
        seen.add(sid)
        step = self.steps_by_id.get(sid)
        if step is None:
            return False
        if step["op"] == "ask":
            return True
        if step["op"] == "assert":
            refs = ([step["step"]] if isinstance(step.get("step"), str) else []) + (step.get("all") or []) + (step.get("any") or [])
            return any(self.roots_in_ask(ref, seen) for ref in refs)
        return False

    def contradicted(self, sid: str, seen: set[str] | None = None) -> bool:
        """Whether a deterministic assert, or one of the asserts it combines, came out against the planner's
        expectation."""
        seen = seen or set()
        if sid in seen:
            return False
        seen.add(sid)
        step = self.steps_by_id.get(sid)
        result = self.results.get(sid)
        if step is None or step["op"] != "assert" or result is None:
            return False
        if result.get("contradicted"):
            return True
        return any(self.contradicted(ref, seen) for ref in (step.get("all") or []) + (step.get("any") or []))

    # -- running --

    def _summary(self, op: str, result: dict[str, Any]) -> str:
        if op in TEST_OPS:
            return result.get("title", "") + ": " + result.get("outcome", "")
        if op == "ask":
            return f"answer {result.get('answer')} (quote: {_clip(result.get('quote') or '', 160)})"
        if op == "assert":
            value = result.get("value")
            text = "holds" if value else "unknown" if value is None else "does not hold"
            return text + (f" ({result['detail']})" if result.get("detail") and not value else "") + (
                "; contradicts the expectation" if result.get("contradicted") else "")
        if op == "finding":
            if result.get("emitted"):
                return "finding emitted"
            return result.get("gate") or result.get("detail") or "no finding"
        title = result.get("title", op)
        if result.get("items"):
            shown = "; ".join(_render(item) for item in result["items"][:5])
            return f"{title}: {shown}" + (" ..." if len(result["items"]) > 5 else "")
        return title

    def _record(self, step: dict[str, Any], result: dict[str, Any], began: float, inputs: Any, **extra: Any) -> None:
        tokens = 0
        if step["op"] == "ask" or extra.get("resolved"):
            label = extra.get("ask") or step["id"]
            tokens = sum(int(item.get("tokens") or 0) for item in self.frames.timings
                         if item.get("phase") == "ask" and item.get("reviewer") == label)
        result["summary"] = self._summary(step["op"], result)
        self.records.append({
            "id": step["id"], "op": step["op"], "status": result["status"], "ms": int((self.clock() - began) * 1000),
            "tokens": tokens, "input": _clip(json.dumps(inputs, default=str), RECORD_CHARS),
            "output": _clip(result.get("text") if result.get("text") else result["summary"], RECORD_CHARS),
            **({"detail": result["detail"]} if result.get("detail") else {}),
            **{key: value for key, value in extra.items() if value is not None},
        })
        self.results[step["id"]] = result

    def _skip(self, step: dict[str, Any], why: str, began: float) -> None:
        self._record(step, {"op": step["op"], "status": "skipped", "detail": why, "text": ""}, began, None)

    def _unavailable_runner(self, args: dict[str, Any]) -> str | None:
        """Why a test step will not run before trying: its runner was unavailable in an earlier run."""
        if self.session is None:
            return None
        paths = args.get("paths") if "paths" in args else args.get("tests")
        try:
            groups = self.session.plan(self.session.paths(paths))
        except testing.TestsRejected:
            return None
        for runner, directory, _relative in groups:
            if (runner.name, directory or ".") in self.unavailable:
                return (f"the {runner.name} runner in {directory or '.'} was unavailable in an earlier run (missing "
                        "dependencies); not tried again")
        return None

    def _lookup(self, step: dict[str, Any], scope: dict[str, Any]) -> None:
        began = self.clock()
        args = self.fill(step["args"], scope)
        try:
            if step["op"] in TEST_OPS and self.session is None:
                raise deep.Rejected("tests are not run in this review")
            if step["op"] in TEST_OPS:
                why = self._unavailable_runner(args if isinstance(args, dict) else {})
                if why:
                    raise deep.Rejected(why)
            title, body = deep.serve_request(self.repo, {step["op"]: args}, self.session)
        except deep.Rejected as error:
            outcome = {"op": step["op"], "status": "failed", "detail": str(error), "text": ""}
            if step["op"] in TEST_OPS:
                outcome.update(status="ok", outcome="could_not_run")
            self._record(step, outcome, began, args)
            return
        result: dict[str, Any] = {"op": step["op"], "status": "ok", "title": title, "text": _clip(body, RESULT_CHARS),
                                  "count": _count_of(step["op"], title, body), "items": _items_of(step["op"], body),
                                  "capped": CAPPED in title, "truncated": len(body) > RESULT_CHARS or "... cut" in body}
        if step["op"] in TEST_OPS:
            number = int(title.split()[-1]) if title.split()[-1].isdigit() else None
            record = next((item for item in self.session.records if item["n"] == number), None)
            result["run"] = record
            status = (record or {}).get("status")
            result["outcome"] = status if status in ("passed", "failed") else "could_not_run"
            if record and record.get("mutation"):
                result["caught"] = bool(record.get("caught"))
            if status == "unavailable" and record is not None:
                self.unavailable.add((record["runner"], record.get("cwd") or "."))
        self._record(step, result, began, args)

    def _assert(self, step: dict[str, Any]) -> None:
        began = self.clock()
        value: bool | None
        detail = ""
        if isinstance(step.get("step"), str):
            target = self.results.get(step["step"])
            kind, op, operand = step["predicate_parsed"]
            if target is None or target["status"] != "ok":
                value, detail = None, f"{step['step']} did not run"
            elif target["op"] in TEST_OPS and target.get("outcome") == "could_not_run" and not (
                    kind == "status" and operand == "could_not_run"):
                value, detail = None, f"{step['step']} could not run"
            elif kind == "count":
                count = target.get("count")
                if count is None:
                    value, detail = None, f"{step['step']} has no count"
                else:
                    value, detail = _count_predicate(count, bool(target.get("capped")), op, int(operand))
            elif kind in ("status", "answer"):
                actual = target.get("outcome") if kind == "status" else target.get("answer")
                value = (actual == operand) if op == "==" else (actual != operand)
                # An unclear answer satisfies nothing but `answer == unclear`.
                if kind == "answer" and actual == "unclear" and not (op == "==" and operand == "unclear"):
                    value = None
                detail = f"{kind} is {actual}"
            else:
                needle = self.fill(op)
                present = _flat(needle) in _flat(target.get("text") or "")
                if not present and target.get("truncated"):
                    value, detail = None, f"{needle!r} is not in the result, which is truncated"
                else:
                    value = present if not operand else not present
                    detail = f"{needle!r} {'is' if present else 'is not'} in the result"
        else:
            refs = step.get("all") or step.get("any") or []
            values = [self.truth(ref) for ref in refs]
            if step.get("all"):
                value = False if any(item is False for item in values) else True if all(item is True for item in values) else None
            else:
                value = True if any(item is True for item in values) else False if all(item is False for item in values) else None
            detail = ", ".join(f"{ref}={'?' if item is None else item}" for ref, item in zip(refs, values))
        expect = step.get("expect")
        contradicted = (expect is not None and value is not None and value != expect
                        and not self.roots_in_ask(step["id"]))
        text = self.fill(step.get("holds") or "") if value and not contradicted else ""
        self._record(step, {"op": "assert", "status": "ok", "value": value, "detail": detail, "holds": text, "text": "",
                            "expected": expect, "contradicted": contradicted},
                     began, step.get("predicate") or {"all": step.get("all"), "any": step.get("any")})

    def _material(self, refs: list[str], left: int) -> tuple[list[str], list[str]]:
        """Result views for the given steps, as labelled data, within `left` characters: (views, texts)."""
        views: list[str] = []
        texts: list[str] = []
        for ref in refs:
            result = self.results.get(ref)
            if result is None or result["status"] != "ok":
                continue
            text = result.get("text") or result.get("summary") or ""
            text = text if len(text) <= left else text[: max(0, left - 1)] + "…"
            left -= len(text)
            views.append(f"Material from step {ref} ({result.get('title', result['op'])}; untrusted repository data "
                         f"read by the host at the reviewed commit):\n{text}")
            texts.append(text)
        return views, texts

    def _ask_views(self, step: dict[str, Any], scope: dict[str, Any]) -> tuple[list[str], str] | None:
        """The views of an ask (question, then each context result as data) and the material the quote must come
        from; None when a context step did not run."""
        if any(self.results.get(ref) is None or self.results[ref]["status"] != "ok" for ref in step["context"]):
            return None
        views, texts = self._material(step["context"], ASK_CONTEXT_CHARS)
        return [f"Question: {self.fill(step['question'], scope)}", *views], "\n".join(texts)

    def _check_quote(self, reply: Any, material: str) -> tuple[str, str, bool, str]:
        """(answer, quote, quote_ok, detail) of a small model's reply, the quote checked against the material."""
        if not isinstance(reply, dict):
            return "unclear", "", False, "no reply"
        answer = str(reply.get("answer", "")).strip().lower()
        quote = _flat(reply.get("quote") if isinstance(reply.get("quote"), str) else "")
        flat = _flat(material)
        quote_ok = len(quote) >= 4 and (quote in flat or any(
            len(line) >= 8 and line in quote for line in (_flat(item) for item in material.splitlines())))
        if answer not in ANSWERS:
            return "unclear", quote, quote_ok, f"the answer {answer!r} is not yes, no or unclear"
        if answer != "unclear" and not quote_ok:
            return "unclear", quote, quote_ok, "the quote is not in the material: the answer counts as unclear"
        return answer, quote, quote_ok, ""

    async def _asks(self, steps: list[tuple[dict[str, Any], dict[str, Any]]]) -> None:
        jobs = []
        prepared = []
        for step, scope in steps:
            began = self.clock()
            if self.asks >= MAX_ASKS:
                self._skip(step, f"the limit of {MAX_ASKS} asks is reached", began)
                continue
            views = self._ask_views(step, scope)
            if views is None:
                self._skip(step, "a context step did not run", began)
                continue
            self.asks += 1
            jobs.append((step["id"], ASK_TASK, views[0]))
            prepared.append((step, views[0], views[1], began))
        if not jobs:
            return
        replies = await self.frames.run("ask", jobs, contract=ASK_CONTRACT, model=self.ask_model,
                                        thinking=self.ask_thinking, cutoff=self.cutoff)
        for (step, views, material, began), reply in zip(prepared, replies):
            if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
                why = getattr(reply, "error", None) or getattr(reply, "status", None) or "no reply"
                self._record(step, {"op": "ask", "status": "failed", "detail": _text(why, 160), "text": ""}, began, views[0])
                continue
            answer, quote, quote_ok, detail = self._check_quote(reply, material)
            self._record(step, {"op": "ask", "status": "ok", "answer": answer, "quote": quote, "quote_ok": quote_ok,
                                "why": _text(reply.get("why"), 300), "detail": detail,
                                "text": f"answer: {answer}\nquote: {quote}\nwhy: {_text(reply.get('why'), 300)}"},
                         began, views[0])

    def _expand(self, step: dict[str, Any]) -> list[dict[str, Any]]:
        """The instances of a for_each template over the items of its `over` result."""
        source = self.results.get(step["over"])
        items = (source.get("items") or []) if source and source["status"] == "ok" else []
        instances: list[dict[str, Any]] = []
        template_ids = {sub["id"] for sub in step["steps"]}
        total = len(self.steps_by_id)
        for index, item in enumerate(items[: step["max_items"]]):
            if total + len(instances) + len(step["steps"]) > self.max_steps:
                self.truncated.append(f"{step['id']}: items from {index} on were not expanded ({self.max_steps} steps at most)")
                break
            scope = {"item": item, "index": index}
            rename = {sid: f"{step['id']}[{index}].{sid}" for sid in template_ids}

            def ref(name: str) -> str:
                return rename.get(name, name)

            for sub in step["steps"]:
                clone = json.loads(json.dumps(sub))
                clone["id"] = rename[sub["id"]]
                clone["needs"] = [ref(name) for name in clone.get("needs") or []]
                if clone.get("when"):
                    clone["when"] = {"step": ref(clone["when"]["step"]), "not": clone["when"]["not"]}
                for key in ("over", "step"):
                    if isinstance(clone.get(key), str):
                        clone[key] = ref(clone[key])
                for key in ("context", "all", "any", "evidence"):
                    if clone.get(key):
                        clone[key] = [ref(name) for name in clone[key]]
                clone["scope"] = scope
                clone["origin"] = step["id"]
                instances.append(clone)
        if len(items) > step["max_items"]:
            self.truncated.append(f"{step['id']}: {len(items) - step['max_items']} items beyond max_items were not visited")
        self.expanded += len(instances)
        return instances

    # -- findings --

    def _resolve_views(self, step: dict[str, Any], scope: dict[str, Any], why: str) -> tuple[list[str], str]:
        """The question the host puts to the small model for a finding whose check is unknown or contradicted: the
        finding, the check as planned and as it came out, and the raw material (the check's steps, the evidence
        steps, the source the finding names, and for a tests finding the nearest test)."""
        when = step["when"]
        public = {"file": self.fill(step["file"], scope), "line": self.fill(step["line"], scope),
                  "claim": self.fill(step["claim"], scope), "why": self.fill(step.get("why") or "", scope),
                  "scenario": self.fill(step.get("scenario") or "", scope)}
        gate = self.steps_by_id.get(when["step"]) or {}
        checks = []
        leaves: list[str] = []
        for sid in [when["step"], *(gate.get("all") or []), *(gate.get("any") or [])]:
            check = self.steps_by_id.get(sid)
            result = self.results.get(sid) or {}
            if check is None or check["op"] != "assert":
                continue
            what = check.get("predicate") or ("all of " + ", ".join(check.get("all") or []) if check.get("all")
                                              else "any of " + ", ".join(check.get("any") or []))
            came = "unknown" if result.get("value") is None else str(result.get("value")).lower()
            checks.append(f"{sid}: {what} on step {check.get('step') or '-'}; expected {str(check.get('expect')).lower()}, "
                          f"came out {came} ({result.get('detail', '')})"
                          + (f"; the planner wrote: {check.get('holds')}" if check.get("holds") else ""))
            if isinstance(check.get("step"), str):
                leaves.append(check["step"])
        negated = " The finding is planned for when the check does NOT hold." if when["not"] else ""
        views = [f"Finding (as the planner wrote it; its check came out {why}):\n{json.dumps(public, indent=1)}",
                 "The check:\n" + "\n".join(checks) + negated]
        refs = list(dict.fromkeys(leaves + list(step.get("evidence") or [])))
        material_views, texts = self._material(refs, ASK_CONTEXT_CHARS)
        views += material_views
        try:
            path = self.repo.path(public["file"])
            line = int(public["line"])
            lines = self.repo.lines(path)
            if lines and 1 <= line <= len(lines):
                window = source_window(lines, line, 12)
                views.append(f"Source of {path} around line {line} (> marks the line the finding names):\n{window}")
                texts.append(window)
        except (deep.Rejected, TypeError, ValueError):
            pass
        unpinned = step.get("unpinned") if isinstance(step.get("unpinned"), dict) else {}
        closest = unpinned.get("closest_test") if isinstance(unpinned.get("closest_test"), dict) else None
        if closest and isinstance(closest.get("path"), str):
            try:
                test_path = self.repo.path(closest["path"])
                test_lines = self.repo.lines(test_path)
                if test_lines:
                    at = min(max(1, int(closest.get("line") or 1)), len(test_lines))
                    window = source_window(test_lines, at, 25)
                    views.append(f"The existing test nearest to it ({test_path}, around line {at}):\n{window}")
                    texts.append(window)
            except (deep.Rejected, TypeError, ValueError):
                pass
        return views, "\n".join(texts)

    def _finding(self, step: dict[str, Any], scope: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any], str] | None:
        """Run a finding step; returns (step, scope, why) when the host must put it to the small model instead."""
        began = self.clock()
        when = step["when"]
        held = self._condition(when)
        gate = self.steps_by_id.get(when["step"])
        if gate is not None and gate["op"] == "assert":
            if self.contradicted(when["step"]):
                return step, scope, "against the planner's expectation"
            if held is None:
                return step, scope, "unknown"
        if held is None:
            self.not_emitted["undecided"] += 1
            self._record(step, {"op": "finding", "status": "ok", "emitted": False, "text": "", "gate": "gate undecided",
                                "detail": f"its condition {when['step']} did not decide (no finding)"}, began, None)
            return None
        if not held:
            self.not_emitted["gateFalse"] += 1
            self._record(step, {"op": "finding", "status": "ok", "emitted": False, "text": "", "gate": "gate false",
                                "detail": f"{when['step']} does not hold (no finding)"}, began, None)
            return None
        self._emit(step, scope, began, None)
        return None

    def _emit(self, step: dict[str, Any], scope: dict[str, Any], began: float, resolution: dict[str, Any] | None,
              **extra: Any) -> None:
        finding, problem = self._compose(step, scope, resolution)
        gate = "gate unknown -> ask: yes" if resolution is not None else "gate true"
        if finding is None:
            self.dropped.append(f"{step['id']}: {problem}")
            self._record(step, {"op": "finding", "status": "ok", "emitted": False, "gate": f"{gate}; dropped: {problem}",
                                "detail": f"dropped: {problem}", "text": ""}, began, None, **extra)
            return
        reason = self.generic(finding) if self.generic is not None else None
        if reason:
            self.generic_dropped.append(reason)
            self._record(step, {"op": "finding", "status": "ok", "emitted": False, "gate": f"{gate}; dropped: {reason}",
                                "detail": f"dropped: {reason}", "text": ""}, began, None, **extra)
            return
        if finding.get("proof") == "refuted":
            self.refuted += 1
            self._record(step, {"op": "finding", "status": "ok", "emitted": False, "text": "",
                                "gate": f"{gate}; refuted by a test",
                                "detail": "refuted: the mutant it names was caught by a test"}, began, None, **extra)
            return
        self.findings.append(finding)
        self._record(step, {"op": "finding", "status": "ok", "emitted": True, "text": "finding emitted: " + json.dumps(
            {key: finding[key] for key in ("file", "line", "level", "claim")}),
            "detail": finding.get("how_verified") or "rests on a small-model answer: goes to the verifier"}, began, None, **extra)

    async def _resolve(self, deferred: list[tuple[dict[str, Any], dict[str, Any], str]]) -> None:
        """Findings whose check is unknown or contradicted: ask the small model with the raw results, emit on yes."""
        jobs = []
        prepared = []
        for step, scope, why in deferred:
            began = self.clock()
            if self.asks >= MAX_ASKS:
                self.not_emitted["undecided"] += 1
                self._record(step, {"op": "finding", "status": "ok", "emitted": False, "text": "", "gate": "gate unknown -> no ask left",
                                    "detail": f"its check came out {why}; the limit of {MAX_ASKS} asks is reached"},
                             began, None, resolved="ask")
                continue
            views, material = self._resolve_views(step, scope, why)
            self.asks += 1
            self.auto_asks += 1
            jobs.append((f"{step['id']}.ask", RESOLVE_TASK, views))
            prepared.append((step, scope, why, views, material, began))
        if not jobs:
            return
        replies = await self.frames.run("ask", jobs, contract=ASK_CONTRACT, model=self.ask_model,
                                        thinking=self.ask_thinking, cutoff=self.cutoff)
        for (step, scope, why, views, material, began), reply in zip(prepared, replies):
            label = f"{step['id']}.ask"
            if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
                error = getattr(reply, "error", None) or getattr(reply, "status", None) or "no reply"
                self._record(step, {"op": "finding", "status": "failed", "text": "",
                                    "detail": f"its check came out {why}; the question to the small model failed ({_text(error, 160)})"},
                             began, views[0], resolved="ask", ask=label)
                continue
            answer, quote, quote_ok, detail = self._check_quote(reply, material)
            self.resolved += 1
            resolution = {"id": label, "answer": answer, "quote": quote, "why": _text(reply.get("why"), 300),
                          "question": f"does the finding hold although its check came out {why}?"}
            if answer == "yes":
                self._emit(step, scope, began, resolution, resolved="ask", ask=label, answer=answer)
            else:
                self.not_emitted["askedNo" if answer == "no" else "askedUnclear"] += 1
                self._record(step, {"op": "finding", "status": "ok", "emitted": False, "text": "",
                                    "gate": f"gate unknown -> ask: {answer}",
                                    "detail": f"its check came out {why}; the small model answered {answer}"
                                    + (f" ({detail})" if detail else "") + (f", quoting `{_clip(quote, 160)}`" if quote else "")},
                             began, views[0], resolved="ask", ask=label, answer=answer)

    def _compose(self, step: dict[str, Any], scope: dict[str, Any],
                 resolution: dict[str, Any] | None = None) -> tuple[dict[str, Any] | None, str | None]:
        """The finding of a `finding` step whose condition holds (or that the small model affirmed), with its
        evidence composed from the results it names: (finding, None) or (None, why it was dropped)."""
        try:
            path = self.repo.path(self.fill(step["file"], scope))
            line = int(self.fill(step["line"], scope))
        except (deep.Rejected, TypeError, ValueError) as error:
            return None, f"no tracked file and line ({error})"
        lines = self.repo.lines(path)
        if not lines or not 1 <= line <= len(lines):
            return None, f"{path} has no line {line}"
        citations, problem, beyond = deep.check_citations(self.repo, self.fill(step["citations"], scope), self.diff_lines)
        if step["citations"] and problem:
            return None, problem
        parts: list[str] = []
        test_run: dict[str, Any] | None = None
        observed = False
        asked = resolution is not None or self.roots_in_ask(step["when"]["step"])
        ask_notes: list[str] = []
        usable = 0
        for ref in step["evidence"]:
            result = self.results.get(ref)
            if result is None or result["status"] != "ok":
                parts.append(f"[{ref}] did not run")
                continue
            usable += 1
            if result["op"] in TEST_OPS and result.get("run"):
                run = result["run"]
                if test_run is None:
                    test_run = run
                observed = observed or result["outcome"] in ("passed", "failed")
                parts.append(f"[{ref}] " + testing.summarize(run).split("\n  output")[0].replace("\n", " "))
            elif result["op"] == "ask":
                asked = True
                ask_notes.append(f"{ref}: {self.fill(self.steps_by_id[ref]['question'], scope)} -> {result['answer']}"
                                 + (f", quoting `{result['quote']}`" if result.get("quote") else ""))
                parts.append(f"[{ref}] small model answered {result['answer']}" + (f" quoting `{_clip(result['quote'], 160)}`"
                                                                                  if result.get("quote") else ""))
            else:
                parts.append(f"[{ref}] {result['summary']}")
        if resolution is not None:
            ask_notes.append(f"{resolution['id']}: {resolution['question']} -> {resolution['answer']}"
                             + (f", quoting `{resolution['quote']}`" if resolution.get("quote") else "")
                             + (f" ({resolution['why']})" if resolution.get("why") else ""))
            parts.append(f"[{resolution['id']}] the check was undecided or contradicted; the small model answered yes"
                         + (f" quoting `{_clip(resolution['quote'], 160)}`" if resolution.get("quote") else ""))
            usable += 1
        if usable == 0 and not citations:
            return None, "no evidence: none of the steps it names produced a result"
        if line not in self.diff_lines.get(path, ()):
            beyond = True
        if test_run is not None:
            beyond = True
        name = str(step.get("level", "")).strip().lower()
        stated = (self.to_level(name) if self.to_level else name if name in LEVELS else OLD_LEVELS.get(name)) or "low"
        category = normalize_category(_text(step.get("category"), 40), "bugs")
        scenario = _text(self.fill(step.get("scenario") or "", scope), 500)
        # Deterministic evidence: critical and high only when a run showed the failure; a count is a gap (medium).
        if asked:
            level = self.cap(stated, category, scenario)
        else:
            level = self.cap(stated, category, scenario, True if observed else None)
            if level in ("critical", "high") and not observed:
                level = "medium"
        # The checked citations are rendered by the result (`_public`) in front of this text.
        cited = "; ".join(f"{item['path']}:{item['line']} `{item['quote']}`" for item in citations)
        evidence = " | ".join(parts)
        finding: dict[str, Any] = {
            "file": path, "line": line, "level": level, "severity": LEVEL_TO_OLD[level], "finder_level": stated,
            "category": category, "claim": _text(self.fill(step["claim"], scope), 300),
            "why": _text(self.fill(step.get("why") or "", scope), 600), "scenario": scenario,
            "suggested_fix": _text(self.fill(step.get("fix") or "", scope), 500), "confidence": 0.9 if not asked else 0.7,
            "reviewers": ["compiled"], "source": f"compiled:{step['id']}", "citations": citations, "beyond_diff": beyond,
            "evidence": _clip(evidence, EVIDENCE_CHARS), "program_step": step["id"],
        }
        if test_run is not None:
            finding["test_run"] = test_run["n"]
            finding["test_evidence"] = testing.summarize(test_run).split("\n  output")[0]
        if self.enrich is not None:
            self.enrich(self.fill({"unpinned": step.get("unpinned"), "consequence": step.get("consequence")}, scope), finding)
        # A tests finding settled by a mutation the program ran: proven when the mutant survived, refuted when caught.
        mutation = next((self.results[ref] for ref in step["evidence"] if ref in self.results
                         and self.results[ref]["op"] == "mutation_check" and self.results[ref]["status"] == "ok"
                         and self.results[ref].get("run")), None)
        if mutation is not None and category == "tests":
            if mutation.get("caught"):
                finding["proof"] = "refuted"
            elif mutation["outcome"] == "passed":
                finding["proof"] = "proven"
                finding.setdefault("unpinned", {"behaviour": "", "change": ""})
        if asked:
            finding["ask_evidence"] = "\n".join(ask_notes)
        else:
            finding["host_confirmed"] = True
            finding["how_verified"] = ("the review program's evidence, produced by the host: "
                                       + _clip(" | ".join(parts) or cited, 300))
        return finding, None

    async def execute(self, step: dict[str, Any]) -> dict[str, Any]:
        """Run one step whose dependencies have all finished (the planner cell's path), and return its result."""
        self.steps_by_id[step["id"]] = step
        scope: dict[str, Any] = {}
        began = self.clock()
        held = self._condition(step.get("when")) if step["op"] != "finding" else True
        if held is None:
            self._skip(step, f"its condition {step['when']['step']} did not decide", began)
        elif held is False:
            self._skip(step, f"its condition {step['when']['step']} does not hold", began)
        elif step["op"] in LOOKUPS or step["op"] in TEST_OPS:
            self._lookup(step, scope)
        elif step["op"] == "assert":
            self._assert(step)
        elif step["op"] == "ask":
            await self._asks([(step, scope)])
        elif step["op"] == "finding":
            undecided = self._finding(step, scope)
            if undecided is not None:
                await self._resolve([undecided])
        elif step["op"] == "for_each":
            await self.run([step])
        return self.results.get(step["id"]) or {"op": step["op"], "status": "skipped"}

    async def run(self, steps: list[dict[str, Any]] | None = None) -> None:
        pending: list[dict[str, Any]] = [dict(step) for step in (self.program.steps if steps is None else steps)]
        for step in pending:
            self.steps_by_id[step["id"]] = step
        while pending:
            ready = [step for step in pending if all(ref in self.results for ref in _references(step))
                     and all(ref in self.results for ref in step.get("instances", []))]
            if not ready:
                for step in pending:
                    self._skip(step, "its dependencies never finished", self.clock())
                break
            asks: list[tuple[dict[str, Any], dict[str, Any]]] = []
            deferred: list[tuple[dict[str, Any], dict[str, Any], str]] = []
            added: list[dict[str, Any]] = []
            for step in ready:
                began = self.clock()
                scope = step.get("scope") or {}
                held = self._condition(step.get("when")) if step["op"] != "finding" else True
                if held is None:
                    self._skip(step, f"its condition {step['when']['step']} did not decide", began)
                elif held is False:
                    self._skip(step, f"its condition {step['when']['step']} does not hold", began)
                elif step["op"] == "for_each":
                    if "instances" not in step:
                        instances = self._expand(step)
                        step["instances"] = [item["id"] for item in instances]
                        for item in instances:
                            self.steps_by_id[item["id"]] = item
                        added += instances
                        if instances:
                            continue  # the for_each finishes when its instances have
                    done = [self.results[sid] for sid in step["instances"]]
                    emitted = sum(1 for item in done if item.get("emitted"))
                    held_count = sum(1 for item in done if item["op"] == "assert" and item.get("value"))
                    self._record(step, {"op": "for_each", "status": "ok", "count": len(step["instances"]), "text": "",
                                        "items": [], "detail": f"{emitted} finding(s) emitted, {held_count} assert(s) held"},
                                 began, {"over": step["over"], "instances": len(step["instances"])})
                elif step["op"] in LOOKUPS or step["op"] in TEST_OPS:
                    self._lookup(step, scope)
                elif step["op"] == "assert":
                    self._assert(step)
                elif step["op"] == "finding":
                    undecided = self._finding(step, scope)
                    if undecided is not None:
                        deferred.append(undecided)
                elif step["op"] == "ask":
                    asks.append((step, scope))
            if asks:
                await self._asks(asks)
            if deferred:
                await self._resolve(deferred)
            finished = {step["id"] for step in ready if step["id"] in self.results}
            pending = [step for step in pending if step["id"] not in finished] + added

    def stats(self) -> dict[str, Any]:
        by_status: dict[str, int] = {}
        for record in self.records:
            by_status[record["status"]] = by_status.get(record["status"], 0) + 1
        deterministic = sum(1 for item in self.findings if item.get("host_confirmed"))
        asserts = [self.results[record["id"]] for record in self.records if record["op"] == "assert" and record["status"] == "ok"]
        return {
            "planned": len([sid for sid in self.steps_by_id if "[" not in sid]), "expanded": self.expanded,
            "executed": by_status.get("ok", 0),
            "failed": by_status.get("failed", 0), "skipped": by_status.get("skipped", 0), "asks": self.asks,
            "autoAsks": self.auto_asks,
            "tests": (len(self.session.records) - self.test_runs_before) if self.session is not None else 0,
            "checks": {"held": sum(1 for item in asserts if item.get("value") is True),
                       "failed": sum(1 for item in asserts if item.get("value") is False),
                       "unknown": sum(1 for item in asserts if item.get("value") is None),
                       "contradicted": sum(1 for item in asserts if item.get("contradicted"))},
            "findings": {"deterministic": deterministic, "asked": len(self.findings) - deterministic,
                         "resolved": self.resolved, "dropped": len(self.dropped) + len(self.generic_dropped),
                         "refuted": self.refuted, "notEmitted": dict(self.not_emitted)},
            "truncated": self.truncated,
            "limits": {"planned": self.max_planned, "expanded": self.max_steps},
        }

    def assurance(self) -> list[str]:
        """What the program checked and found to hold: the host's counts, then the `holds` sentences of the asserts
        that were true as expected (first in program order)."""
        lookups = sum(1 for record in self.records if record["op"] in LOOKUPS and record["status"] == "ok")
        asserts = [self.results[record["id"]] for record in self.records if record["op"] == "assert" and record["status"] == "ok"]
        held = [result for result in asserts if result.get("value") is True and not result.get("contradicted")]
        unknown = sum(1 for result in asserts if result.get("value") is None)
        runs = (len(self.session.records) - self.test_runs_before) if self.session is not None else 0
        first = (f"A review program of {len(self.records)} steps ran against the reviewed commit: {lookups} repository "
                 f"lookup{'' if lookups == 1 else 's'}, {runs} test run{'' if runs == 1 else 's'}, {self.asks} small-model "
                 f"question{'' if self.asks == 1 else 's'}; {len(held)} of {len(asserts)} check{'' if len(asserts) == 1 else 's'} held"
                 + (f", {unknown} could not be decided" if unknown else "") + ".")
        sentences = []
        for result in held:
            text = _text(result.get("holds"), 220)
            if text and len(sentences) < MAX_HOLDS:
                sentences.append(text if text.endswith((".", "!", "?")) else text + ".")
        return [first, *sentences]


# --- The planner as a REPL: sandboxed cells over the rv API ------------------------------------------------------

#: The child process that runs the planner's cells. It is launched inside the test sandbox (bubblewrap or unshare:
#: no network, no home, no repository, an empty writable directory) as `python3 -I -c <this>`, and speaks JSON lines
#: on stdin/stdout: the host sends {"cell": code}; the child answers every `rv.<name>(...)` with {"call", "args",
#: "kwargs"} and waits for {"value"} or {"error"}; it ends the cell with {"done": true, "output", "error"}. Cells run
#: with a reduced set of builtins (no open, no exec/eval, imports from a short allowlist); `--probe` lifts that for
#: the isolation self-test only.
CELL_RUNNER = r'''
import sys, json, io, traceback, builtins
_in, _out = sys.stdin, sys.stdout
PROBE = sys.argv[1:] == ["--probe"]
def _send(obj):
    _out.write(json.dumps(obj, default=str) + "\n"); _out.flush()
def _recv():
    line = _in.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)
class RvError(Exception):
    pass
class _Rv:
    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        def call(*args, **kwargs):
            _send({"call": name, "args": list(args), "kwargs": kwargs})
            reply = _recv()
            if "error" in reply:
                raise RvError(reply["error"])
            return reply.get("value")
        call.__name__ = name
        return call
rv = _Rv()
_ALLOWED = {"re", "json", "math", "collections", "itertools", "functools", "textwrap", "string", "operator",
            "statistics", "difflib"}
_real_import = builtins.__import__
def _import(name, globals=None, locals=None, fromlist=(), level=0):
    if level == 0 and name.split(".")[0] in _ALLOWED:
        return _real_import(name, globals, locals, fromlist, level)
    raise ImportError("import of %r is not available in a planner cell: the repository is reached through rv" % name)
_SAFE_NAMES = ("abs", "all", "any", "bool", "chr", "dict", "divmod", "enumerate", "filter", "float", "format",
               "frozenset", "hash", "int", "isinstance", "issubclass", "iter", "len", "list", "map", "max", "min",
               "next", "ord", "print", "range", "repr", "reversed", "round", "set", "slice", "sorted", "str", "sum",
               "tuple", "zip", "Exception", "ValueError", "TypeError", "KeyError", "IndexError", "StopIteration",
               "AttributeError", "RuntimeError", "ZeroDivisionError", "NameError", "ImportError", "AssertionError",
               "LookupError", "ArithmeticError", "NotImplementedError", "__build_class__")
_SAFE = {name: getattr(builtins, name) for name in _SAFE_NAMES if hasattr(builtins, name)}
_SAFE["__import__"] = _import
_globals = {"__builtins__": builtins if PROBE else _SAFE, "rv": rv, "RvError": RvError, "__name__": "__cell__"}
while True:
    message = _recv()
    code = message.get("cell") or ""
    buffer = io.StringIO()
    error = ""
    previous = sys.stdout
    sys.stdout = buffer
    try:
        exec(compile(code, "<cell>", "exec"), _globals)
    except SystemExit:
        error = "SystemExit is not available in a planner cell: call rv.done() instead"
    except BaseException as exc:  # noqa: BLE001
        tail = traceback.extract_tb(sys.exc_info()[2])[-1:]
        where = " (line %d of the cell)" % tail[0].lineno if tail and tail[0].filename == "<cell>" else ""
        error = "".join(traceback.format_exception_only(type(exc), exc)).strip() + where
    finally:
        sys.stdout = previous
    _send({"done": True, "output": buffer.getvalue()[-8000:], "error": error})
'''


class CellRunner:
    """A sandboxed Python child that runs the planner's cells and relays its `rv` calls to a handler."""

    def __init__(self, sandbox: Any, *, python: str = "python3", probe: bool = False) -> None:
        self.sandbox = sandbox
        self.python = python
        self.probe = probe
        self.process: Any = None
        self.workdir: str | None = None

    async def start(self) -> None:
        self.workdir = tempfile.mkdtemp(prefix="ultron-autoreview-cell-")
        command = [self.python, "-I", "-c", CELL_RUNNER] + (["--probe"] if self.probe else [])
        argv = self.sandbox.wrap(command, self.workdir, testing.sandbox_env()) if self.sandbox is not None else command
        self.process = await asyncio.create_subprocess_exec(
            *argv, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            cwd=self.workdir, env=testing._launch_env())

    async def run_cell(self, code: str, handler: Callable[[str, list[Any], dict[str, Any]], Any],
                       timeout_s: float = CELL_TIMEOUT_S) -> tuple[str, str]:
        """Run one cell; `handler(name, args, kwargs)` (async) answers each rv call. Returns (output, error)."""
        assert self.process is not None and self.process.stdin is not None and self.process.stdout is not None
        self.process.stdin.write((json.dumps({"cell": code}) + "\n").encode("utf-8"))
        await self.process.stdin.drain()
        while True:
            try:
                line = await asyncio.wait_for(self.process.stdout.readline(), timeout_s)
            except asyncio.TimeoutError:
                await self.close()
                return "", f"the cell did not finish within {timeout_s:.0f} s and the planner's sandbox was closed"
            if not line:
                stderr = b""
                if self.process.stderr is not None:
                    try:
                        stderr = await asyncio.wait_for(self.process.stderr.read(), 2)
                    except asyncio.TimeoutError:
                        pass
                await self.close()
                return "", "the planner's sandbox exited: " + _text(stderr.decode("utf-8", "replace"), 300)
            try:
                message = json.loads(line.decode("utf-8", "replace"))
            except ValueError:
                continue
            if message.get("done"):
                return str(message.get("output") or "")[-CELL_OUTPUT_CHARS:], str(message.get("error") or "")
            if "call" in message:
                try:
                    value = await handler(str(message["call"]), list(message.get("args") or []), dict(message.get("kwargs") or {}))
                    reply: dict[str, Any] = {"value": value}
                except RvError as error:
                    reply = {"error": str(error)}
                except Exception as error:  # noqa: BLE001  a host fault is reported to the cell, not raised into it
                    reply = {"error": f"host error: {type(error).__name__}: {_text(str(error), 200)}"}
                self.process.stdin.write((json.dumps(reply, default=str) + "\n").encode("utf-8"))
                await self.process.stdin.drain()

    async def close(self) -> None:
        process, self.process = self.process, None
        if process is not None:
            try:
                process.kill()
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(process.wait(), 5)
            except (asyncio.TimeoutError, ProcessLookupError):
                pass
        if self.workdir:
            shutil.rmtree(self.workdir, ignore_errors=True)
            self.workdir = None


class RvError(Exception):
    """An rv call the host refuses; the message is raised inside the cell as RvError."""


def _public_value(result: dict[str, Any]) -> dict[str, Any]:
    """What a cell gets back from an rv call: the step's result, bounded."""
    out: dict[str, Any] = {"id": result.get("id"), "status": result.get("status")}
    for key in ("count", "capped", "truncated", "answer", "quote", "why", "value", "contradicted", "detail", "emitted",
                "caught", "title"):
        if key in result and result[key] is not None:
            out[key] = result[key]
    if result.get("op") in TEST_OPS:
        out["status"] = result.get("outcome")
    if result.get("items"):
        out["items"] = result["items"][:CELL_VALUE_ITEMS]
    if result.get("text"):
        out["text"] = _clip(result["text"], CELL_VALUE_CHARS)
    if result.get("op") == "finding":
        out["gate"] = result.get("gate") or ("finding emitted" if result.get("emitted") else "")
    return out


class CellSession:
    """The `rv` API as the host serves it: every call becomes a step of the shared Interpreter, with the same
    validation, limits and accounting as the JSON program; plus coverage bookkeeping and the end of planning."""

    def __init__(self, interpreter: Interpreter, coverage: list[dict[str, str]], limits: tuple[int, int]) -> None:
        self.interpreter = interpreter
        self.coverage = coverage
        self.limits = limits
        self.covered: set[str] = set()
        self.declared: list[str] = []
        self.done = False
        self.counter = 0
        self.cells: list[str] = []
        #: Cells the planner still has after the current one; told back in every result.
        self.cells_left = 0
        self.materialised = 0

    def _id(self, given: Any) -> str:
        if given is not None:
            if not isinstance(given, str) or not _ID.match(given):
                raise RvError(f"id must match {_ID.pattern}")
            if given in self.interpreter.steps_by_id:
                raise RvError(f"id {given!r} is already used")
            return given
        while True:
            self.counter += 1
            sid = f"s{self.counter}"
            if sid not in self.interpreter.steps_by_id:
                return sid

    def _step(self, raw: dict[str, Any]) -> dict[str, Any]:
        """Validate one synthesized step against the language and the steps so far."""
        total = len(self.interpreter.steps_by_id)
        if total >= self.limits[1] or len([sid for sid in self.interpreter.steps_by_id if "[" not in sid]) >= self.limits[0]:
            raise RvError(f"the step limit is reached ({self.limits[0]} steps as written, {self.limits[1]} after expansion)")
        errors: list[str] = []
        step = _normalize_step(raw, errors)
        if step is not None:
            by_id = dict(self.interpreter.steps_by_id)
            by_id[step["id"]] = step
            _check_step(step, by_id, set(by_id), errors)
            if step["op"] == "ask" and self.interpreter.asks >= MAX_ASKS:
                errors.append(f"the limit of {MAX_ASKS} asks is reached")
        if errors or step is None:
            raise RvError("; ".join(errors) or "invalid step")
        return step

    async def call(self, name: str, args: list[Any], kwargs: dict[str, Any]) -> Any:
        if self.done:
            raise RvError("rv.done() was already called")
        if name == "help":
            return RV_API
        if name == "done":
            missing = uncovered_items(self.coverage, list(self.interpreter.steps_by_id.values()),
                                      self.declared + [f"{item}:" for item in self.covered])
            if missing:
                return {"ok": False, "uncovered": [f"{item['id']}: {item['text']}" for item in missing],
                        "hint": "cover each with a check (covers=[id]) or rv.uncovered(id, why), then call rv.done() again",
                        "cells_left": self.cells_left}
            orphans = self.decided_without_finding()
            if orphans:
                return {"ok": False, "undecided_findings": orphans,
                        "hint": "these asks answered yes but no finding rests on them: attach one with rv.finding(when=<id>, ...) "
                                "or state why there is nothing to report with rv.uncovered-style text in a print, then call rv.done() again",
                        "cells_left": self.cells_left}
            self.done = True
            return {"ok": True}
        if name == "cover":
            item = str(args[0] if args else kwargs.get("item", ""))
            if item not in {entry["id"] for entry in self.coverage}:
                raise RvError(f"{item!r} is not a coverage item")
            self.covered.add(item)
            return {"ok": True}
        if name == "uncovered":
            item = str(args[0] if args else kwargs.get("item", ""))
            why = str(args[1] if len(args) > 1 else kwargs.get("why", ""))
            if item not in {entry["id"] for entry in self.coverage}:
                raise RvError(f"{item!r} is not a coverage item")
            self.declared.append(f"{item}: {why}")
            return {"ok": True}
        if name == "run_program":
            program = args[0] if args else kwargs.get("program")
            validated, errors = validate(program, None, self.limits[0])
            if validated is None:
                raise RvError("the program is invalid: " + "; ".join(errors[:8]))
            clash = [step["id"] for step in validated.steps if step["id"] in self.interpreter.steps_by_id]
            if clash:
                raise RvError("these ids are already used: " + ", ".join(clash))
            self.declared += validated.uncovered
            await self.interpreter.run(validated.steps)
            return {"ok": True, "steps": [_public_value(dict(self.interpreter.results[s["id"]], id=s["id"]))
                                          for s in validated.steps if s["id"] in self.interpreter.results]}
        attached = kwargs.pop("finding", None) if name in ("ask", "assert_", "assert") else None
        if attached is not None and not isinstance(attached, dict):
            raise RvError("finding= takes a dict with the finding's fields (file, line, level, category, claim, ...)")
        raw = self._raw(name, args, kwargs)
        step = self._step(raw)
        result = await self.interpreter.execute(step)
        value = _public_value(dict(result, id=step["id"]))
        if attached is not None:
            # The finding travels with the check that establishes it: emitted by the host when the check decides in
            # its favour, put to the small model when the check is unknown or contradicted, as rv.finding would.
            spec = dict(attached)
            negate = bool(spec.pop("not_", False) or spec.pop("negate", False))
            spec.pop("when", None)
            finding_raw = self._raw("finding", [], {"when": step["id"], "not_": negate, **spec})
            finding_step = self._step(finding_raw)
            finding_result = await self.interpreter.execute(finding_step)
            value["finding"] = _public_value(dict(finding_result, id=finding_step["id"]))
        value["cells_left"] = self.cells_left
        return value

    def decided_without_finding(self) -> list[str]:
        """Asks answered yes that no finding step rests on (in its when or evidence)."""
        used: set[str] = set()
        for step in self.interpreter.steps_by_id.values():
            if step["op"] == "finding":
                used.add(step["when"]["step"])
                used.update(step.get("evidence") or [])
        out = []
        for sid, result in self.interpreter.results.items():
            if result.get("op") == "ask" and result.get("status") == "ok" and result.get("answer") == "yes" and sid not in used:
                out.append(sid)
        return out

    def _place(self, ask_id: str) -> tuple[str, int] | None:
        """A file and line for a finding materialised from an ask: the first read in its context, else the first
        grep hit, else the line of its quote in one of those files."""
        step = self.interpreter.steps_by_id.get(ask_id) or {}
        for ref in step.get("context") or []:
            source = self.interpreter.steps_by_id.get(ref) or {}
            result = self.interpreter.results.get(ref) or {}
            args = source.get("args") or {}
            if source.get("op") == "read" and isinstance(args.get("path"), str):
                try:
                    return self.interpreter.repo.path(args["path"]), max(1, int(args.get("start") or 1))
                except (deep.Rejected, TypeError, ValueError):
                    continue
            for item in result.get("items") or []:
                if isinstance(item, dict) and isinstance(item.get("path"), str) and isinstance(item.get("line"), int):
                    try:
                        return self.interpreter.repo.path(item["path"]), item["line"]
                    except deep.Rejected:
                        continue
        return None

    async def materialise(self) -> list[str]:
        """When the cells run out: every ask answered yes that carries no finding becomes a candidate through the
        resolve path (the small model judges it with the ask's question, answer and quote and the planner's own
        words as the claim); nothing decided is dropped silently. Returns notes."""
        notes = []
        for ask_id in self.decided_without_finding():
            ask = self.interpreter.steps_by_id[ask_id]
            result = self.interpreter.results[ask_id]
            place = self._place(ask_id)
            if place is None:
                notes.append(f"ask {ask_id} was answered yes but no finding rests on it and it names no file to place one at")
                continue
            path, line = place
            raw = {"id": f"{ask_id}_f", "op": "finding", "when": {"step": ask_id}, "file": path, "line": line,
                   "level": "medium", "category": "correctness", "claim": _text(ask["question"], 300),
                   "why": _text(f"The small model answered yes: {result.get('why') or ''}", 600),
                   "scenario": "", "evidence": [ask_id, *(ask.get("context") or [])], "citations": []}
            errors: list[str] = []
            step = _normalize_step(raw, errors)
            if step is None or errors:
                notes.append(f"ask {ask_id} could not be materialised as a finding: " + "; ".join(errors[:2]))
                continue
            self.interpreter.steps_by_id[step["id"]] = step
            await self.interpreter._resolve([(step, {}, "materialised from an ask answered yes that the planner left without a finding")])
            self.materialised += 1
        return notes

    def _raw(self, name: str, args: list[Any], kwargs: dict[str, Any]) -> dict[str, Any]:
        """One rv call as the JSON step it stands for."""
        kwargs = dict(kwargs)
        sid = self._id(kwargs.pop("id", None))
        covers = kwargs.pop("covers", None)
        if name in LOOKUPS or name in TEST_OPS:
            names = {"read": ("path", "start", "end"), "grep": ("pattern", "glob", "start", "end", "count_only", "max"),
                     "list": ("dir",), "definition": ("symbol",), "references": ("symbol",), "history": ("path", "n"),
                     "blame_range": ("path", "start", "end"), "pickaxe": ("string", "n"), "run_tests": ("paths", "select"),
                     "mutation_check": ("path", "line", "replacement", "tests")}[name]
            params = dict(zip(names, args))
            params.update(kwargs)
            if "glob" in params:
                params["path_glob"] = params.pop("glob")
            params = {key: value for key, value in params.items() if value is not None}
            return {"id": sid, "op": name, "args": params, "covers": covers}
        if name == "ask":
            params = dict(zip(("question", "context"), args))
            params.update(kwargs)
            return {"id": sid, "op": "ask", "question": params.get("question"), "context": params.get("context"), "covers": covers}
        if name in ("assert_", "assert"):
            params = dict(zip(("step", "predicate", "expect", "holds"), args))
            params.update(kwargs)
            return {"id": sid, "op": "assert", "step": params.get("step"), "predicate": params.get("predicate"),
                    "expect": params.get("expect"), "holds": params.get("holds") or "", "all": params.get("all"),
                    "any": params.get("any"), "covers": covers}
        if name == "finding":
            params = dict(zip(("when", "file", "line", "level", "category", "claim", "why", "fix", "scenario", "evidence"), args))
            params.update(kwargs)
            when = params.get("when")
            if isinstance(when, str):
                when = {"step": when, "not": bool(params.get("not_") or params.get("negate"))}
            return {"id": sid, "op": "finding", "when": when, "covers": covers,
                    **{key: params.get(key) for key in ("file", "line", "level", "category", "claim", "why", "fix", "scenario",
                                                        "evidence", "citations", "unpinned", "consequence")}}
        raise RvError(f"rv has no method {name!r}; rv.help() lists them")


async def run_cells(frames: Any, *, views: list[str], task: str, plan_model: str | None, plan_thinking: str | None,
                    cutoff: float | None, clock: Callable[[], float], interpreter: Interpreter,
                    coverage: list[dict[str, str]], limits: tuple[int, int], sandbox: Any, cells: int,
                    python: str = "python3") -> dict[str, Any]:
    """The planner as a REPL: up to `cells` frames, each returning one Python cell the host runs in the sandboxed
    child; the cell's output goes back to the planner. Returns the session and what went wrong, if anything."""
    session = CellSession(interpreter, coverage, limits)
    runner = CellRunner(sandbox, python=python)
    transcript: list[str] = []
    notes: list[str] = []
    began = clock()
    status = "ok"
    cell_tokens: list[int] = []

    def plan_tokens() -> int:
        return sum(int(item.get("tokens") or 0) for item in frames.timings if item.get("phase") == "plan")

    def shown_transcript() -> str:
        # The last cells in full; one line per earlier cell. The step records stay host-side.
        parts = []
        for index, entry in enumerate(transcript):
            if index >= len(transcript) - CELL_TRANSCRIPT_FULL:
                parts.append(entry)
            else:
                first = next((line for line in entry.split("\n")[1:] if line.strip()), "")
                calls = entry.count("rv.")
                parts.append(f"{entry.split(chr(10))[0]} (summary) {_clip(first.strip(), 100)} ... {calls} rv call(s)"
                             + (" with an error" if "\nError: " in entry else ""))
        return "\n\n".join(parts)[-CELL_TRANSCRIPT_CHARS:]
    try:
        await runner.start()
    except Exception as error:  # noqa: BLE001
        return {"session": session, "status": "failed", "error": f"the planner's sandbox could not start: {_text(str(error), 200)}",
                "notes": notes, "ms": int((clock() - began) * 1000)}
    try:
        for number in range(1, cells + 1):
            last = number == cells
            session.cells_left = cells - number
            shown = shown_transcript()
            extra = [f"Cells so far and their output (cell {number} of at most {cells}; {cells - number} left after this one"
                     + (", the last" if last else "") + "):\n" + (shown or "(none yet)")]
            if last:
                extra.append("This is your last cell: emit a finding for every decided check that supports one (rv.finding, "
                             "or finding= on the check) and call rv.done(). Asks answered yes without a finding are put to "
                             "the small model as candidates by the host, but with your words they would be better.")
            before = plan_tokens()
            replies = await frames.run("plan", [("planner", task, views + extra)], contract=CELL_CONTRACT, model=plan_model,
                                       thinking=plan_thinking, cutoff=cutoff)
            cell_tokens.append(plan_tokens() - before)
            reply = replies[0]
            if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
                why = getattr(reply, "error", None) or getattr(reply, "status", None) or "no reply"
                status = "failed"
                notes.append(f"the planner frame failed at cell {number} ({_text(why, 160)})")
                break
            code = reply.get("cell") if isinstance(reply.get("cell"), str) else ""
            if not code.strip():
                transcript.append(f"Cell {number}: (empty)")
                if reply.get("done") is True:
                    break
                continue
            session.cells.append(code)
            output, error = await runner.run_cell(code, session.call)
            transcript.append(f"Cell {number}:\n{code}\nOutput:\n{output or '(no output)'}" + (f"\nError: {error}" if error else ""))
            if runner.process is None:
                notes.append(error or "the planner's sandbox exited")
                status = "failed"
                break
            if session.done or reply.get("done") is True:
                break
        if not session.done and status == "ok":
            notes.append(f"the planner did not call rv.done() within {cells} cells")
        notes += await session.materialise()
    finally:
        await runner.close()
    return {"session": session, "status": status, "notes": notes, "ms": int((clock() - began) * 1000),
            "transcript": transcript, "cell_tokens": cell_tokens}


CELL_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {"cell": {"type": "string"}, "done": {"type": "boolean"}},
    "required": ["cell", "done"],
}


# --- Orchestration ---------------------------------------------------------------------------------------------


def runner_availability(session: Any) -> tuple[list[str], set[tuple[str, str]]]:
    """What the automatic run showed per runner and directory: lines for the planner, and the (runner, directory)
    pairs found unavailable (test steps for them are could_not_run at once)."""
    if session is None:
        return [], set()
    seen: dict[tuple[str, str], str] = {}
    for record in session.records:
        key = (record["runner"], record.get("cwd") or ".")
        if key not in seen or record["status"] != "passed":
            seen[key] = record["status"]
    lines = []
    unavailable: set[tuple[str, str]] = set()
    for (runner, directory), status in seen.items():
        where = f"{runner} in {directory}"
        if status == "unavailable":
            unavailable.add((runner, directory))
            lines.append(f"{where}: unavailable (missing dependencies; run_tests and mutation_check on its files will "
                         "not run: read the tests and ask instead)")
        else:
            lines.append(f"{where}: available (the automatic run {status})")
    if not lines and session.runner is not None:
        lines.append(f"{session.runner.name} (the repository's runner, {session.runner.because}): not tried yet")
    return lines, unavailable


def retrieve(repo: deep.Repo, brief: Any, files: list[FileDiff], *, clock: Callable[[], float],
             limit: int = RETRIEVAL_CHARS) -> dict[str, Any]:
    """The mechanical lookups the deep pass's investigators had to ask for, done by the host before the planner
    runs: for every changed or added symbol its references (capped) and the test files that mention it with their
    parametrize/fixture lines; for every new config key, field, flag or environment variable the sibling family
    (where the other keys of its declaration are registered, and whether the new one is there too); and the
    definitions of the helpers the new code calls. Most relevant first (symbols with the most references, keys
    whose siblings have a registry), bounded by `limit` characters. Returns {"text", "items", "chars", "ms"}."""
    began = clock()
    changed_lines = {item.path: {line.new for hunk in item.hunks for line in hunk.lines if line.kind == "+" and line.new}
                     for item in files}
    changed_paths = {item.path for item in files}
    extracted = getattr(brief, "extracted", {}) or {}
    required = getattr(brief, "required", {}) or {}
    sections: list[tuple[int, str]] = []
    items = 0

    def outside(hits: list[tuple[str, int, str]]) -> list[tuple[str, int, str]]:
        return [hit for hit in hits if hit[1] not in changed_lines.get(hit[0], ())]

    for name in list(getattr(brief, "symbols", []) or [])[:8]:
        hits = outside(repo.grep(name, fixed=True, word=True, limit=60))
        if not hits:
            sections.append((0, f"References of `{name}` outside the changed lines: none."))
            items += 1
            continue
        tests = [hit for hit in hits if deep._TEST.search(hit[0])]
        code = [hit for hit in hits if hit not in tests]
        lines = [f"References of `{name}` outside the changed lines ({len(hits)} in all, {len(tests)} in tests):"]
        lines += [f"  {p}:{n}: {_clip(text.strip(), 160)}" for p, n, text in code[:RETRIEVAL_HITS]]
        if len(code) > RETRIEVAL_HITS:
            lines.append(f"  ... {len(code) - RETRIEVAL_HITS} more")
        for test_path in list(dict.fromkeys(hit[0] for hit in tests))[:3]:
            mentions = [f"{n}: {_clip(text.strip(), 120)}" for p, n, text in tests if p == test_path][:4]
            structure = repo.grep(deep._STRUCTURE, pathspec=test_path, limit=6)
            lines.append(f"  test {test_path} mentions it at " + "; ".join(mentions))
            if structure:
                lines.append("    structure: " + "; ".join(f"{n}: {_clip(text.strip(), 100)}" for _, n, text in structure))
        sections.append((len(hits), "\n".join(lines)))
        items += 1
    keys = list(dict.fromkeys(list(required.get("claims") or []) + list(extracted.get("fields") or [])
                              + list(extracted.get("constants") or [])))[:8]
    # The siblings of a new field are the other fields declared in the same changed files (the declaration
    # list it joins), plus the other new keys.
    declared: list[str] = []
    for item in files:
        for line in repo.lines(item.path) or []:
            match = deep._FIELD.match(line)
            if match and match.group(1) not in declared:
                declared.append(match.group(1))
    for key in keys:
        own = outside(repo.grep(key, fixed=True, word=True, limit=40))
        siblings = [other for other in list(dict.fromkeys(keys + declared)) if other != key][:12]
        family: list[tuple[str, int, str]] = []
        if siblings:
            pattern = "|".join(re.escape(other) for other in siblings)
            family = [hit for hit in repo.grep(pattern, limit=80) if hit[0] not in changed_paths or hit[1] not in changed_lines.get(hit[0], ())]
        # A registry is a file where at least two sibling names appear; the new key counts as present when it
        # appears anywhere in that file at the reviewed commit.
        by_file: dict[str, set[str]] = {}
        for p, _n, text in family:
            by_file.setdefault(p, set()).update(other for other in siblings if re.search(r"(?<![A-Za-z0-9_])" + re.escape(other) + r"(?![A-Za-z0-9_])", text))
        present = {p for p, _n, _t in repo.grep(key, fixed=True, word=True, limit=60)}
        registries = [p for p, names in sorted(by_file.items(), key=lambda pair: -len(pair[1])) if len(names) >= 2]
        lines = [f"New key `{key}`: used outside the changed lines at "
                 + ("; ".join(f"{p}:{n}" for p, n, _t in own[:RETRIEVAL_HITS]) or "nowhere") + "."]
        for p in registries[:5]:
            where = "; ".join(f"{n}: {_clip(text.strip(), 100)}" for q, n, text in family if q == p)[:400]
            lines.append(f"  siblings ({len(by_file[p])}) registered in {p}" + (": the new key is NOT there" if p not in present
                                                                        else ": the new key is there too") + f" ({where})")
        sections.append((100 + len(registries) * 10, "\n".join(lines)))
        items += 1
    for name in list(extracted.get("calls") or [])[:5]:
        if name in (getattr(brief, "helpers", []) or []):
            continue  # the brief already shows it
        hits = repo.grep(deep._definition_pattern(name), limit=2)
        if hits:
            p, n, _t = hits[0]
            lines = repo.lines(p)
            if lines:
                sections.append((1, f"Called by the change, `{name}` is defined at {p}:{n}:\n{deep._numbered(lines, n, min(len(lines), n + 11))}"))
                items += 1
    text = ""
    for _rank, section in sorted(sections, key=lambda pair: -pair[0]):
        if len(text) + len(section) + 2 > limit:
            text += "\n... (retrieved context cut at its size limit)"
            break
        text += ("\n\n" if text else "") + section
    return {"text": text, "items": items, "chars": len(text), "ms": int((clock() - began) * 1000)}


def planner_views(diff_text: str, brief_text: str, *, context: str, intent: str, guidance: str, tests_block: str,
                  runners: list[str], tests_allowed: bool, runs_left: int, coverage: list[dict[str, str]] | None = None,
                  limits: tuple[int, int] = (MAX_PROGRAM_STEPS, MAX_STEPS), retrieved: str = "") -> tuple[list[str], bool]:
    """The planner's views: the diff, the brief, the context, the intent, the guides, the coverage the program
    must have, the limits, the test situation."""
    cut = len(diff_text) > deep.DIFF_CHARS
    views = [f"The diff under review (new-file line numbers in the gutter):\n{diff_text[:deep.DIFF_CHARS]}"
             + ("\n... (diff cut at its size limit; read the files for the rest)" if cut else ""),
             f"Investigation brief, built by the host from the repository at the reviewed commit:\n{brief_text}"]
    if retrieved:
        views.append("Retrieved context, looked up by the host at the reviewed commit (untrusted repository data): the "
                     "references, tests and sibling families of the changed names, and the helpers the change calls.\n\n" + retrieved)
    for part in (context, intent, guidance, tests_block):
        if part:
            views.append(part)
    if coverage:
        views.append("Coverage the program must have (name the id in a step's \"covers\", or list it in the program's "
                     "\"uncovered\" as \"<id>: why\"; T-items are catalogue shapes whose trigger is in this change):\n"
                     + "\n".join(f"- {item['id']}: {item['text']}" for item in coverage))
    views.append(f"Limits: {limits[0]} steps as written, {limits[1]} after for_each expansion, {MAX_ASKS} asks.")
    if tests_allowed:
        views.append(f"Tests may run: yes; executions left: {runs_left}. Test runners:\n"
                     + ("\n".join(f"- {line}" for line in runners) or "- none recognized"))
    else:
        views.append("Tests may run: no.")
    return views, cut


async def run_compiled(frames: Any, files: list[FileDiff], read_file: Callable[[str], list[str] | None], *, root: str,
                       rev: str, diff_text: str, context: str, intent: str, guidance: str, plan_model: str | None,
                       plan_thinking: str | None, ask_model: str | None, ask_thinking: str | None,
                       cutoff: float | None, clock: Callable[[], float], cap: Callable[..., str],
                       runner: Any = None, tests: dict[str, Any] | None = None,
                       to_level: Callable[[Any], str | None] | None = None, title: str = "", description: str = "",
                       base: str | None = None, enrich: Callable[[Any, dict[str, Any]], None] | None = None,
                       generic: Callable[[dict[str, Any]], str | None] | None = None,
                       program: Any = None, dump_path: str | None = None, plan_style: str = DEFAULT_PLAN_STYLE,
                       plan_cells: int = DEFAULT_PLAN_CELLS, cell_sandbox: Any = "detect",
                       python: str = "python3") -> dict[str, Any]:
    """Map, retrieve, plan (the planner as sandboxed cells over the rv API, or one planner frame, or `program` given
    to replay), validate (one repair round for a frame), interpret.
    Returns findings (deterministic ones `host_confirmed`), what was dropped, the step records, the stats, the
    assurance, the test report and, when the program could not be had, `fallback`: why the caller should run the
    `both` mode instead."""
    repo = deep.Repo(root, rev, runner)
    if not repo.files():
        raise RuntimeError("the reviewed commit could not be read")
    brief = deep.build_brief(repo, files, read_file, title=title, description=description, base=base)
    diff_lines = {item.path: {line.new for hunk in item.hunks for line in hunk.lines if line.new is not None}
                  for item in files}
    changed_lines = sum(item.added + item.removed for item in files)
    limits = limits_for(changed_lines)
    coverage = coverage_items(brief, files)
    retrieved = retrieve(repo, brief, files, clock=clock)
    session = None
    planner: dict[str, Any] = {"ms": 0, "tokens": 0, "repairs": 0, "status": "replayed" if program is not None else "ok",
                               "style": "replay" if program is not None else plan_style, "cells": 0}
    out: dict[str, Any] = {"findings": [], "dropped": [], "generic": [], "records": [], "stats": None, "assurance": [],
                           "repo": repo, "brief": brief, "program": None, "fallback": None, "diff_cut": False,
                           "planner": planner, "tests": {"enabled": tests is not None, "mechanism": None, "note": None, "runs": [],
                                                         "env": "none", "toolchain": []},
                           "refuted": 0, "uncovered": [], "coverage": coverage, "limits": limits, "notes": [],
                           "retrieval": {key: retrieved[key] for key in ("items", "chars", "ms")}}
    try:
        started = deep.start_tests(repo, files, tests, brief, root=root, rev=rev, clock=clock)
        session = started["session"]
        out["tests"]["mechanism"] = started["mechanism"]
        out["tests"]["note"] = started["note"]
        out["tests"]["env"] = started["env"]
        out["tests"]["toolchain"] = started["toolchain"]
        out["findings"] = list(started["observed"])
        tests_allowed = session is not None and session.limit > len(session.records)
        runners, unavailable = runner_availability(session)
        views, cut = planner_views(
            diff_text, brief.text, context=context, intent=intent, guidance=guidance, tests_block=deep.tests_block(started),
            runners=runners, tests_allowed=tests_allowed, coverage=coverage, limits=limits, retrieved=retrieved["text"],
            runs_left=max(0, session.limit - len(session.records)) if session is not None else 0)
        out["diff_cut"] = cut
        validated: Program | None = None
        errors: list[str] = []
        if program is None and plan_style == "cell":
            # The planner as a REPL: its cells run in the test sandbox (no network, no home, no repository); without a
            # sandbox the planner runs as one frame instead.
            sandbox = cell_sandbox
            if sandbox == "detect":
                sandbox = started["sandbox"] if started.get("sandbox") is not None else testing.detect_sandbox(
                    image=(tests or {}).get("image"))
            if sandbox is None:
                out["notes"] = ["the planner ran as one frame: no sandbox is available for planner cells"]
                plan_style = "frame"
                planner["style"] = "frame"
            else:
                interpreter = Interpreter(Program([], ""), repo, frames=frames, session=session, diff_lines=diff_lines,
                                          ask_model=ask_model, ask_thinking=ask_thinking, cutoff=cutoff, clock=clock, cap=cap,
                                          to_level=to_level, enrich=enrich, generic=generic, unavailable=unavailable,
                                          max_steps=limits[1])
                began = clock()
                cell_run = await run_cells(frames, views=views, task=cell_planner_task(tests_allowed), plan_model=plan_model,
                                           plan_thinking=plan_thinking, cutoff=cutoff, clock=clock, interpreter=interpreter,
                                           coverage=coverage, limits=limits, sandbox=sandbox,
                                           cells=max(1, min(MAX_PLAN_CELLS, int(plan_cells))), python=python)
                cell_session: CellSession = cell_run["session"]
                planner["ms"] = int((clock() - began) * 1000)
                planner["tokens"] = sum(int(item.get("tokens") or 0) for item in frames.timings if item.get("phase") == "plan")
                planner["cells"] = len(cell_session.cells)
                planner["cellTokens"] = list(cell_run.get("cell_tokens") or [])
                planner["status"] = cell_run["status"]
                out["notes"] = list(cell_run.get("notes") or [])
                if cell_run["status"] == "failed" and not cell_session.cells:
                    out["fallback"] = cell_run.get("error") or "; ".join(cell_run["notes"]) or "the planner cells failed"
                    return out
                steps = [step for sid, step in interpreter.steps_by_id.items() if "[" not in sid]
                declared = list(cell_session.declared)
                for item in uncovered_items(coverage, steps, declared + [f"{sid}:" for sid in cell_session.covered]):
                    declared.append(f"{item['id']}: not covered by the planner")
                validated = Program(steps, "", declared)
                out["program"] = {**validated.as_json(), "style": "cell", "cells": cell_session.cells}
                if dump_path:
                    with open(dump_path, "w", encoding="utf-8") as handle:
                        json.dump(out["program"], handle, indent=1)
                out["findings"] += interpreter.findings
                out["dropped"] = interpreter.dropped
                out["generic"] = interpreter.generic_dropped
                out["records"] = interpreter.records
                out["refuted"] = interpreter.refuted
                by_id = {item["id"]: item for item in coverage}
                for item in declared:
                    cid, _, why = item.partition(":")
                    text = by_id.get(cid.strip(), {}).get("text", cid.strip())
                    out["uncovered"].append(f"{cid.strip()} ({text})" + (f": {why.strip()}" if why.strip() else ""))
                stats = interpreter.stats()
                stats["findings"]["materialised"] = cell_session.materialised
                out["stats"] = {**stats, "planner": planner, "summary": "", "retrieval": out["retrieval"],
                                "coverage": {"items": len(coverage), "covered": len(coverage) - len(declared),
                                             "uncovered": [item.split(":", 1)[0].strip() for item in declared]}}
                out["assurance"] = interpreter.assurance()
                runs = list(session.records) if session is not None else []
                out["tests"]["runs"] = [{key: value for key, value in record.items() if key != "output"} | {"output": record["output"][-600:]}
                                        for record in runs]
                return out
        if program is not None:
            validated, errors = validate(program, coverage, limits[0])
            if validated is None:
                out["fallback"] = "the given program is invalid: " + "; ".join(errors[:4])
        else:
            task = compiled_planner_task(tests_allowed)
            began = clock()
            for attempt in range(2):
                extra = [] if attempt == 0 else [COMPILED_REPAIR.format(errors="\n".join(f"- {item}" for item in errors[:20]))]
                replies = await frames.run("plan", [("planner", task, views + extra)], contract=PROGRAM_CONTRACT,
                                           model=plan_model, thinking=plan_thinking, cutoff=cutoff)
                reply = replies[0]
                if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
                    why = getattr(reply, "error", None) or getattr(reply, "status", None) or "no reply"
                    out["fallback"] = f"the planner frame failed ({_text(why, 160)})"
                    planner["status"] = "failed"
                    break
                validated, errors = validate(reply, coverage, limits[0])
                if validated is not None:
                    break
                if attempt == 0:
                    planner["repairs"] = 1
                else:
                    out["fallback"] = "the program was invalid after one repair: " + "; ".join(errors[:4])
                    planner["status"] = "invalid"
            planner["ms"] = int((clock() - began) * 1000)
            planner["tokens"] = sum(int(item.get("tokens") or 0) for item in frames.timings if item.get("phase") == "plan")
        if validated is None:
            return out
        out["program"] = validated.as_json()
        if dump_path:
            with open(dump_path, "w", encoding="utf-8") as handle:
                json.dump(out["program"], handle, indent=1)
        interpreter = Interpreter(validated, repo, frames=frames, session=session, diff_lines=diff_lines, ask_model=ask_model,
                                  ask_thinking=ask_thinking, cutoff=cutoff, clock=clock, cap=cap, to_level=to_level,
                                  enrich=enrich, generic=generic, unavailable=unavailable, max_steps=limits[1])
        await interpreter.run()
        by_id = {item["id"]: item for item in coverage}
        for item in validated.uncovered:
            cid, _, why = item.partition(":")
            text = by_id.get(cid.strip(), {}).get("text", cid.strip())
            out["uncovered"].append(f"{cid.strip()} ({text})" + (f": {why.strip()}" if why.strip() else ""))
        out["findings"] += interpreter.findings
        out["dropped"] = interpreter.dropped
        out["generic"] = interpreter.generic_dropped
        out["records"] = interpreter.records
        out["refuted"] = interpreter.refuted
        out["stats"] = {**interpreter.stats(), "planner": planner, "summary": validated.summary, "retrieval": out["retrieval"],
                        "coverage": {"items": len(coverage), "covered": len(coverage) - len(validated.uncovered),
                                     "uncovered": [item.split(":", 1)[0].strip() for item in validated.uncovered]}}
        out["assurance"] = interpreter.assurance()
        runs = list(session.records) if session is not None else []
        out["tests"]["runs"] = [{key: value for key, value in record.items() if key != "output"} | {"output": record["output"][-600:]}
                                for record in runs]
    finally:
        if session is not None:
            session.close()
    return out
