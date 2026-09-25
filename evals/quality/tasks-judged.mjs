/**
 * Frozen JUDGED quality tasks: work without an exact checker (a refactor's quality, an explanation of a module, a
 * design note), scored by an LLM judge against a frozen rubric next to a light deterministic sanity check.
 *
 * Same contract as tasks.mjs (`files`, `prompts`, `hidden`, `verify`), plus `judge`:
 *   judge: {
 *     rubric: [{ id, points, description }],   // frozen criteria; the judge gives each an integer 0..points
 *     inputs: [path],                          // files the judge reads after the run (the seed version too, if any)
 *     passAt: 0..1,                            // normalized score at or above which the judge calls the run good
 *     model?: "provider/model",                // overrides --judge-model for this task (none do: one fixed judge)
 *   }
 * `verify` stays the pass/fail of record: it only proves the output exists and is sane (behaviour preserved, the
 * document is there, long enough, and has the required sections). The judge score is reported beside it and never
 * gates a release on its own (scripts/gate.mjs reads only the default set's deterministic pass rates).
 * Reference solutions live in tasks-judged-solutions.mjs; `--self-check` runs the deterministic checks on them and
 * validates each rubric's wiring with a fake judge (no model calls).
 * Do not edit a task or rubric after measurements exist: add a new id instead (see FROZEN_AT).
 */

import { createHash } from "node:crypto";

export const FROZEN_AT = "2026-09-26-judged";

const py = (source) => `${source.replace(/^\n/, "")}\n`;

const REPORT_ORIGINAL = py(`
def render_report(orders):
    # orders: list of dicts with id, customer, status, items (list of {sku, qty, unit_cents}), optional coupon
    lines = []
    total_all = 0
    count_shipped = 0
    count_pending = 0
    count_cancelled = 0
    for o in orders:
        if o["status"] == "shipped":
            count_shipped = count_shipped + 1
            t = 0
            for it in o["items"]:
                t = t + it["qty"] * it["unit_cents"]
            if o.get("coupon") == "SAVE10":
                t = t - t * 10 // 100
            elif o.get("coupon") == "SAVE20":
                t = t - t * 20 // 100
            if t > 100000:
                t = t - 500
            total_all = total_all + t
            lines.append("#" + str(o["id"]) + " " + o["customer"] + " shipped " + str(t // 100) + "." + str(t % 100).zfill(2))
        elif o["status"] == "pending":
            count_pending = count_pending + 1
            t = 0
            for it in o["items"]:
                t = t + it["qty"] * it["unit_cents"]
            if o.get("coupon") == "SAVE10":
                t = t - t * 10 // 100
            elif o.get("coupon") == "SAVE20":
                t = t - t * 20 // 100
            if t > 100000:
                t = t - 500
            lines.append("#" + str(o["id"]) + " " + o["customer"] + " pending " + str(t // 100) + "." + str(t % 100).zfill(2))
        elif o["status"] == "cancelled":
            count_cancelled = count_cancelled + 1
            lines.append("#" + str(o["id"]) + " " + o["customer"] + " cancelled")
        else:
            raise ValueError("bad status " + str(o["status"]))
    lines.append("shipped: " + str(count_shipped) + ", pending: " + str(count_pending) + ", cancelled: " + str(count_cancelled))
    lines.append("revenue: " + str(total_all // 100) + "." + str(total_all % 100).zfill(2))
    return "\\n".join(lines)
`);

const REPORT_CLI = py(`
import json
import sys

from report import render_report

if __name__ == "__main__":
    print(render_report(json.load(open(sys.argv[1]))))
`);

const REPORT_CHECK = py(`
"""Sanity check for refactor-order-report: behaviour identical to the frozen original, and actually decomposed."""
import ast
import random
import shutil
import sys

# Never compare against bytecode cached from an earlier version of report.py.
shutil.rmtree("__pycache__", ignore_errors=True)
sys.dont_write_bytecode = True

import _report_original as original
import report

MAX_FUNCTION_LINES = 25


def orders(random_):
    result = []
    for index in range(random_.randint(0, 12)):
        items = [
            {"sku": f"s{random_.randint(1, 9)}", "qty": random_.randint(1, 40), "unit_cents": random_.randint(1, 9000)}
            for _ in range(random_.randint(0, 5))
        ]
        order = {"id": index, "customer": random_.choice(["ana", "bo", "cy"]), "status": random_.choice(["shipped", "pending", "cancelled"]), "items": items}
        coupon = random_.choice([None, None, "SAVE10", "SAVE20", "OTHER"])
        if coupon:
            order["coupon"] = coupon
        result.append(order)
    return result


def outcome(module, value):
    try:
        return ("ok", module.render_report(value))
    except Exception as error:  # the error for a bad status is part of the behaviour
        return (type(error).__name__, str(error))


random_ = random.Random(7)
cases = [orders(random_) for _ in range(600)]
cases.append([{"id": 1, "customer": "ana", "status": "lost", "items": []}])
cases.append([{"id": 2, "customer": "bo", "status": "shipped", "items": [{"sku": "x", "qty": 1, "unit_cents": 200000}], "coupon": "SAVE20"}])
for value in cases:
    expected, got = outcome(original, value), outcome(report, value)
    if expected != got:
        sys.exit(f"behaviour changed for {value!r}:\\nexpected {expected!r}\\ngot      {got!r}")

tree = ast.parse(open("report.py").read())
functions = [node for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))]
if len(functions) < 3:
    sys.exit(f"expected the report to be decomposed into at least 3 functions, found {len(functions)}")
for node in functions:
    length = node.end_lineno - node.lineno + 1
    if length > MAX_FUNCTION_LINES:
        sys.exit(f"{node.name} is {length} lines long (at most {MAX_FUNCTION_LINES})")
print("ok")
`);

const BUCKET = py(`
"""Token-bucket rate limiting."""
import time


class TokenBucket:
    """Allows bursts up to \`capacity\`, refilling at \`rate\` tokens per second."""

    def __init__(self, capacity, rate, clock=time.monotonic):
        if capacity <= 0 or rate <= 0:
            raise ValueError("capacity and rate must be positive")
        self.capacity = float(capacity)
        self.rate = float(rate)
        self._clock = clock
        self._tokens = float(capacity)
        self._last = clock()

    def _refill(self):
        now = self._clock()
        elapsed = max(0.0, now - self._last)
        self._last = max(self._last, now)
        self._tokens = min(self.capacity, self._tokens + elapsed * self.rate)

    def allow(self, cost=1):
        """Take \`cost\` tokens if available. A cost above capacity can never succeed and consumes nothing."""
        if cost <= 0:
            raise ValueError("cost must be positive")
        self._refill()
        if cost > self.capacity or cost > self._tokens:
            return False
        self._tokens -= cost
        return True

    def retry_after(self, cost=1):
        """Seconds until \`allow(cost)\` could succeed; None if it never can."""
        if cost > self.capacity:
            return None
        self._refill()
        missing = cost - self._tokens
        return 0.0 if missing <= 0 else missing / self.rate
`);

const REGISTRY = py(`
"""Per-key buckets with least-recently-used eviction."""
from .bucket import TokenBucket


class BucketRegistry:
    def __init__(self, capacity, rate, max_keys=10_000, clock=None):
        self._args = (capacity, rate)
        self._clock = clock
        self._max_keys = max_keys
        self._buckets = {}

    def allow(self, key, cost=1):
        bucket = self._buckets.pop(key, None)
        if bucket is None:
            if len(self._buckets) >= self._max_keys:
                oldest = next(iter(self._buckets))
                del self._buckets[oldest]
            bucket = TokenBucket(*self._args, **({"clock": self._clock} if self._clock else {}))
        self._buckets[key] = bucket  # re-insert: dict order is least recently used first
        return bucket.allow(cost)
`);

const RATELIMIT_INIT = py(`
from .bucket import TokenBucket
from .registry import BucketRegistry

__all__ = ["TokenBucket", "BucketRegistry"]
`);

/** Deterministic check for a written document: present, bounded length, required words and headings, code untouched. */
function documentCheck({ path, minWords, maxWords, mustMention, headings, minHeadings = 0, unchanged }) {
	return py(`
import hashlib
import os
import re
import sys

path = ${JSON.stringify(path)}
if not os.path.exists(path):
    sys.exit(f"{path} was not written")
text = open(path, encoding="utf-8").read()
words = len(re.findall(r"\\S+", text))
if not ${minWords} <= words <= ${maxWords}:
    sys.exit(f"{path} has {words} words (expected ${minWords}-${maxWords})")
for name in ${JSON.stringify(mustMention)}:
    if name.lower() not in text.lower():
        sys.exit(f"{path} never mentions {name}")
titles = [line.lstrip("#").strip().lower() for line in text.splitlines() if re.match(r"^#{1,4} ", line)]
for pattern in ${JSON.stringify(headings)}:
    if not any(re.search(pattern, title) for title in titles):
        sys.exit(f"{path} has no heading matching {pattern!r}; headings: {titles}")
if len(titles) < ${minHeadings}:
    sys.exit(f"{path} has {len(titles)} headings (expected at least ${minHeadings})")
for name, digest in ${JSON.stringify(unchanged)}.items():
    if hashlib.sha256(open(name, "rb").read()).hexdigest() != digest:
        sys.exit(f"{name} was modified; the task was to write a document only")
print("ok")
`);
}

const sha256 = (text) => createHash("sha256").update(text).digest("hex");

const PRICING_CONTEXT = `# checkout-service: pricing context

- checkout-service renders the cart and takes payment. It calls pricing-api for every cart view, one request per cart.
- pricing-api latency: p50 800 ms, p99 3 s. Hard rate limit: 50 requests/s per client; beyond it, HTTP 429.
- Prices change at most once every 5 minutes. A displayed price may be up to 5 minutes stale; the amount charged at
  payment must use a price fetched at payment time.
- Traffic: 400 cart views/s at peak, 30k distinct SKUs, 90% of views touch only 2k SKUs. A cart has 1-15 SKUs.
- pricing-api accepts batches: one request can price up to 50 SKUs.
- checkout-service runs as 6 stateless instances behind a load balancer. A shared Redis cluster is available.
- Last month pricing-api was down for 12 minutes and checkout returned errors for the whole outage.
`;

export function tasks() {
	return [
		{
			id: "refactor-order-report",
			category: "judged-refactor",
			files: { "report.py": REPORT_ORIGINAL, "cli.py": REPORT_CLI },
			prompts: [
				"report.py works but is hard to maintain. Refactor it for readability and maintainability without changing its behaviour: render_report(orders) must return exactly the same text (and raise the same error for an unknown status) for every input, and cli.py must keep working. Keep it in report.py.",
			],
			hidden: { "_report_original.py": REPORT_ORIGINAL, "check_report_hidden.py": REPORT_CHECK },
			verify: "python3 check_report_hidden.py",
			judge: {
				inputs: ["report.py"],
				passAt: 0.7,
				rubric: [
					{
						id: "behavior_preserved",
						points: 2,
						description:
							"render_report keeps its signature and output exactly, including the ValueError for an unknown status; no new dependencies.",
					},
					{
						id: "duplication_removed",
						points: 3,
						description:
							"The duplicated order-total, coupon and large-order rebate computation and the money formatting each exist once (helpers), not once per status.",
					},
					{
						id: "naming_and_constants",
						points: 2,
						description:
							"Names are descriptive (no o/t/it); the coupon rates, the 100000-cent threshold and the 500-cent rebate are named constants or a table; control flow is flat.",
					},
					{
						id: "scope_discipline",
						points: 2,
						description:
							"It stays a refactor: no new features, no speculative abstraction or framework, no changed output format.",
					},
					{
						id: "reviewer_verdict",
						points: 1,
						description: "A careful reviewer would approve it as clearly easier to maintain than the original.",
					},
				],
			},
		},
		{
			id: "explain-ratelimit-module",
			category: "judged-explanation",
			files: {
				"ratelimit/__init__.py": RATELIMIT_INIT,
				"ratelimit/bucket.py": BUCKET,
				"ratelimit/registry.py": REGISTRY,
			},
			prompts: [
				"Write EXPLANATION.md explaining the ratelimit package to a new team member: what it is for, how the token-bucket algorithm works here (including the refill arithmetic), the public API of TokenBucket and BucketRegistry, the edge cases the code handles, and how to test it deterministically. Use markdown headings. Do not change any code.",
			],
			hidden: {
				"check_explanation_hidden.py": documentCheck({
					path: "EXPLANATION.md",
					minWords: 200,
					maxWords: 2000,
					mustMention: ["TokenBucket", "BucketRegistry", "allow", "retry_after"],
					headings: [],
					minHeadings: 3,
					unchanged: {
						"ratelimit/bucket.py": sha256(BUCKET),
						"ratelimit/registry.py": sha256(REGISTRY),
						"ratelimit/__init__.py": sha256(RATELIMIT_INIT),
					},
				}),
			},
			verify: "python3 check_explanation_hidden.py",
			judge: {
				inputs: ["EXPLANATION.md", "ratelimit/bucket.py", "ratelimit/registry.py"],
				passAt: 0.7,
				rubric: [
					{
						id: "refill_accuracy",
						points: 3,
						description:
							"Explains refill correctly: tokens = min(capacity, tokens + elapsed * rate), the bucket starts full, and elapsed is clamped at 0 so a clock going backwards adds nothing.",
					},
					{
						id: "edge_cases",
						points: 2,
						description:
							"Names the handled edge cases: a cost above capacity always fails and consumes nothing (retry_after returns None); non-positive cost, capacity or rate raise ValueError; a refused allow consumes nothing.",
					},
					{
						id: "registry",
						points: 2,
						description:
							"Explains BucketRegistry correctly: one bucket per key, least-recently-used eviction at max_keys through dict re-insertion, and an evicted key starts again with a full bucket.",
					},
					{
						id: "testing",
						points: 1,
						description: "Shows how to test deterministically by injecting a fake clock.",
					},
					{
						id: "clarity_no_invention",
						points: 2,
						description:
							"Clear for a newcomer, and invents no API, parameter or behaviour that the code does not have.",
					},
				],
			},
		},
		{
			id: "design-note-price-cache",
			category: "judged-design",
			files: { "CONTEXT.md": PRICING_CONTEXT },
			prompts: [
				"Read CONTEXT.md and write DESIGN.md: a design note proposing how checkout-service should cache prices. Use these sections as markdown headings: Goals and non-goals, Proposed design, Invalidation and freshness, Failure modes, Alternatives considered, Rollout and metrics. Do not write code.",
			],
			hidden: {
				"check_design_hidden.py": documentCheck({
					path: "DESIGN.md",
					minWords: 400,
					maxWords: 2500,
					mustMention: ["pricing-api", "Redis", "429"],
					headings: ["goal", "design", "invalidation|freshness", "failure", "alternative", "rollout"],
					unchanged: { "CONTEXT.md": sha256(PRICING_CONTEXT) },
				}),
			},
			verify: "python3 check_design_hidden.py",
			judge: {
				inputs: ["DESIGN.md", "CONTEXT.md"],
				passAt: 0.7,
				rubric: [
					{
						id: "constraints",
						points: 3,
						description:
							"Respects the stated constraints with checked numbers: at most 5 minutes staleness for display, a fresh price at payment, and an upstream rate that stays under 50 requests/s at 400 cart views/s peak (e.g. shared cache, batching, hot-set warming); uses the 6 instances and Redis sensibly.",
					},
					{
						id: "failure_modes",
						points: 2,
						description:
							"Covers a stampede on expiry, a pricing-api outage (serve stale within stated bounds; what payment does), a Redis outage, and 429 handling.",
					},
					{
						id: "alternatives",
						points: 2,
						description:
							"Weighs at least two real alternatives (for example per-instance cache only, invalidation pushed by pricing, pre-warming the hot 2k SKUs) with honest trade-offs.",
					},
					{
						id: "rollout_metrics",
						points: 2,
						description:
							"A concrete rollout (flag, shadow or percentage) and metrics (hit rate, upstream request rate, staleness, error rate) with success criteria.",
					},
					{
						id: "concision",
						points: 1,
						description: "Concrete and decision-oriented; no filler or generic advice.",
					},
				],
			},
		},
	];
}
