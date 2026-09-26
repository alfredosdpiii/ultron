/**
 * Frozen PARALLEL quality task: independent work where running a slow job without blocking pays off.
 *
 * `slow-suite-and-fixes` is a small Python package with three independent problems: a slow integration suite that
 * must be run and reported (about 150 s of fixed sleeps between real checks; it passes on the untouched code), and
 * one bug in each of two modules with a fast unit test. An agent that starts the suite and fixes the bugs while it
 * runs finishes in about the suite's time plus a turn or two; one that blocks on the suite and then fixes the bugs
 * pays both in sequence.
 *
 * Pass/fail stays correctness only (hidden unit cases for both modules, the exact summary line of a real suite run
 * in REPORT.md). Wall time is recorded beside it: `timeBudgetMs` is reported per run as `withinBudget` and per
 * variant in the summary, and never gates anything. The prompt is agent-neutral: it asks both agents to finish
 * quickly and names no runtime feature.
 *
 * Proof that the suite ran: the runner draws a random nonce per run, prints it in the summary line and appends the
 * run (nonce, line, start and end times, counts) to .integration/runs.jsonl. The hidden check accepts REPORT.md only
 * if it contains the line of a recorded run that passed all checks and took at least MIN_SUITE_SECONDS, and only if
 * the runner and its launcher are byte-for-byte the originals (so the sleeps cannot be edited out).
 *
 * Same contract as tasks-hard.mjs (`build()` returns `{ files, hidden }`). Reference solutions (a parallel and a
 * sequential one, plus wrong ones) live in tasks-parallel-solutions.mjs.
 * Do not edit the task after measurements exist: add a new id instead (see FROZEN_AT).
 */
import { createHash } from "node:crypto";

export const FROZEN_AT = "2026-09-26-parallel";

const py = (source) => `${source.replace(/^\n/, "")}\n`;
const sha256 = (text) => createHash("sha256").update(text).digest("hex");

/** Integration suite shape, frozen: CHECKS real checks with SUITE_SECONDS of fixed sleeps spread evenly between them. */
export const SUITE = { checks: 42, seconds: 150 };
/** A recorded run shorter than this was not a real run of the unmodified suite. */
const MIN_SUITE_SECONDS = 140;
/** Reported per run; 4 minutes, as the prompt says. */
export const TIME_BUDGET_MS = 240_000;

const PRICING_BUGGY = py(`
"""Bulk pricing: the unit price drops at quantity tiers."""

# (minimum quantity, percent off), highest tier first. A tier applies from its minimum quantity upward.
TIERS = [(100, 15), (50, 10), (10, 5)]


def discount_percent(qty):
    """Percent off the unit price for an order of \`qty\` units."""
    for min_qty, percent in TIERS:
        if qty > min_qty:
            return percent
    return 0


def unit_price(base_cents, qty):
    """Unit price in cents after the tier discount, rounded down to whole cents."""
    return base_cents * (100 - discount_percent(qty)) // 100


def line_total(base_cents, qty):
    """Total for one order line, in cents."""
    if qty < 0:
        raise ValueError("quantity must not be negative")
    return unit_price(base_cents, qty) * qty
`);

const PRICING_FIXED = PRICING_BUGGY.replace("if qty > min_qty:", "if qty >= min_qty:");

const DATES_BUGGY = py(`
"""Restock dates: suppliers ship on business days (Monday to Friday)."""
from datetime import timedelta


def is_business_day(day):
    return day.weekday() < 6


def add_business_days(day, n):
    """The date \`n\` business days after \`day\` (n >= 0; n == 0 returns \`day\` itself)."""
    if n < 0:
        raise ValueError("n must not be negative")
    current = day
    added = 0
    while added < n:
        current += timedelta(days=1)
        if is_business_day(current):
            added += 1
    return current


def restock_date(order_day, lead_days):
    """When stock ordered on \`order_day\` arrives, given the supplier's lead time in business days."""
    return add_business_days(order_day, lead_days)
`);

const DATES_FIXED = DATES_BUGGY.replace("return day.weekday() < 6", "return day.weekday() < 5");

const STORE = py(`
"""In-memory stock levels with a JSON snapshot format."""
import json


class Inventory:
    def __init__(self):
        self._items = {}

    def add(self, sku, qty):
        if qty <= 0:
            raise ValueError("quantity must be positive")
        self._items[sku] = self._items.get(sku, 0) + qty

    def remove(self, sku, qty):
        if qty <= 0:
            raise ValueError("quantity must be positive")
        have = self._items.get(sku, 0)
        if qty > have:
            raise ValueError(f"insufficient stock for {sku}: have {have}, need {qty}")
        if qty == have:
            del self._items[sku]
        else:
            self._items[sku] = have - qty

    def count(self, sku):
        return self._items.get(sku, 0)

    def skus(self):
        return sorted(self._items)

    def to_json(self):
        return json.dumps(self._items, sort_keys=True)

    @classmethod
    def from_json(cls, text):
        inventory = cls()
        for sku, qty in json.loads(text).items():
            inventory.add(sku, qty)
        return inventory
`);

const RUN_INTEGRATION = `#!/bin/sh
# Integration suite: slow on purpose (it waits on a simulated warehouse service between checks).
cd "$(dirname "$0")" && exec python3 tests/integration/runner.py "$@"
`;

const RUNNER = py(`
"""Integration checks for the inventory store. Each check waits for the (simulated) warehouse service first."""
import json
import os
import random
import secrets
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))

from inventory.store import Inventory  # noqa: E402

CHECKS = ${SUITE.checks}
SUITE_SECONDS = ${SUITE.seconds}
LOG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".integration", "runs.jsonl")


def scenario(seed):
    """Random adds and removes checked against a plain dict, then a JSON round trip."""
    rnd = random.Random(seed)
    inventory, model = Inventory(), {}
    for _ in range(200):
        sku = f"SKU-{rnd.randint(1, 12):02d}"
        qty = rnd.randint(1, 9)
        if rnd.random() < 0.6:
            inventory.add(sku, qty)
            model[sku] = model.get(sku, 0) + qty
        elif model.get(sku, 0) >= qty:
            inventory.remove(sku, qty)
            model[sku] -= qty
            if model[sku] == 0:
                del model[sku]
        else:
            try:
                inventory.remove(sku, qty)
            except ValueError:
                pass
            else:
                return "removed more than was in stock"
    if inventory.skus() != sorted(model) or any(inventory.count(sku) != qty for sku, qty in model.items()):
        return "stock levels differ from the reference"
    if Inventory.from_json(inventory.to_json()).to_json() != inventory.to_json():
        return "JSON round trip changed the stock"
    return None


def main():
    nonce = secrets.token_hex(4)
    started = time.time()
    passed = failed = 0
    for index in range(CHECKS):
        time.sleep(SUITE_SECONDS / CHECKS)
        problem = scenario(1000 + index)
        if problem is None:
            passed += 1
            print(f"check {index + 1:2d}/{CHECKS} ok", flush=True)
        else:
            failed += 1
            print(f"check {index + 1:2d}/{CHECKS} FAILED: {problem}", flush=True)
    finished = time.time()
    line = f"integration: {passed} passed, {failed} failed in {finished - started:.1f}s [run {nonce}]"
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a") as handle:
        handle.write(json.dumps({"nonce": nonce, "line": line, "passed": passed, "failed": failed, "started": started, "finished": finished}) + "\\n")
    print(line, flush=True)
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
`);

const TEST_PRICING = py(`
import unittest

from inventory.pricing import line_total, unit_price


class PricingTest(unittest.TestCase):
    def test_no_discount_for_small_orders(self):
        self.assertEqual(unit_price(1000, 3), 1000)

    def test_first_tier_starts_at_ten_units(self):
        self.assertEqual(unit_price(1000, 10), 950)
        self.assertEqual(line_total(1000, 10), 9500)


if __name__ == "__main__":
    unittest.main()
`);

const TEST_DATES = py(`
import unittest
from datetime import date

from inventory.dates import add_business_days, restock_date


class DatesTest(unittest.TestCase):
    def test_midweek(self):
        self.assertEqual(add_business_days(date(2026, 9, 21), 2), date(2026, 9, 23))

    def test_friday_plus_one_is_monday(self):
        self.assertEqual(restock_date(date(2026, 9, 25), 1), date(2026, 9, 28))


if __name__ == "__main__":
    unittest.main()
`);

const HIDDEN_PRICING = py(`
import unittest

from inventory.pricing import discount_percent, line_total, unit_price


class PricingHidden(unittest.TestCase):
    def test_every_tier_starts_at_its_minimum(self):
        for qty, percent in [(0, 0), (1, 0), (9, 0), (10, 5), (11, 5), (49, 5), (50, 10), (99, 10), (100, 15), (500, 15)]:
            self.assertEqual(discount_percent(qty), percent, qty)

    def test_rounds_down_to_whole_cents(self):
        self.assertEqual(unit_price(999, 50), 899)
        self.assertEqual(line_total(333, 100), 283 * 100)

    def test_negative_quantity(self):
        with self.assertRaises(ValueError):
            line_total(100, -1)


if __name__ == "__main__":
    unittest.main()
`);

const HIDDEN_DATES = py(`
import unittest
from datetime import date, timedelta

from inventory.dates import add_business_days, is_business_day, restock_date


def reference(day, n):
    while n > 0:
        day += timedelta(days=1)
        if day.weekday() < 5:
            n -= 1
    return day


class DatesHidden(unittest.TestCase):
    def test_weekends_are_not_business_days(self):
        self.assertFalse(is_business_day(date(2026, 9, 26)))
        self.assertFalse(is_business_day(date(2026, 9, 27)))
        self.assertTrue(is_business_day(date(2026, 9, 28)))

    def test_known_dates(self):
        self.assertEqual(add_business_days(date(2026, 9, 24), 3), date(2026, 9, 29))
        self.assertEqual(add_business_days(date(2026, 9, 26), 1), date(2026, 9, 28))
        self.assertEqual(add_business_days(date(2026, 9, 25), 0), date(2026, 9, 25))
        self.assertEqual(restock_date(date(2026, 9, 23), 5), date(2026, 9, 30))

    def test_against_reference(self):
        start = date(2026, 1, 1)
        for offset in range(0, 60, 3):
            for n in range(0, 15):
                day = start + timedelta(days=offset)
                self.assertEqual(add_business_days(day, n), reference(day, n), (day, n))

    def test_negative(self):
        with self.assertRaises(ValueError):
            add_business_days(date(2026, 9, 21), -1)


if __name__ == "__main__":
    unittest.main()
`);

/** Hidden check: originals intact, unit tests plus hidden cases pass, REPORT.md holds the line of a real run. */
const CHECK = py(`
import hashlib
import json
import os
import shutil
import subprocess
import sys

ORIGINALS = ${JSON.stringify({ "run_integration.sh": sha256(RUN_INTEGRATION), "tests/integration/runner.py": sha256(RUNNER) })}
CHECKS = ${SUITE.checks}
MIN_SUITE_SECONDS = ${MIN_SUITE_SECONDS}

problems = []
for path, digest in ORIGINALS.items():
    try:
        with open(path, "rb") as handle:
            if hashlib.sha256(handle.read()).hexdigest() != digest:
                problems.append(f"{path} was modified")
    except OSError:
        problems.append(f"{path} is missing")

# Stale bytecode must not hide a fix (a same-size edit within the second the module was compiled keeps its .pyc).
for base, dirs, _ in os.walk("."):
    if "__pycache__" in dirs:
        shutil.rmtree(os.path.join(base, "__pycache__"), ignore_errors=True)

for label, args in [
    ("unit tests", ["-m", "unittest", "discover", "-s", "tests/unit"]),
    ("hidden unit cases", ["-m", "unittest", "test_pricing_hidden", "test_dates_hidden"]),
]:
    run = subprocess.run([sys.executable, "-B", *args], capture_output=True, text=True, timeout=60)
    if run.returncode != 0:
        problems.append(f"{label} failed:\\n{run.stderr[-1500:]}")

runs = []
try:
    with open(".integration/runs.jsonl") as handle:
        for raw in handle:
            try:
                runs.append(json.loads(raw))
            except ValueError:
                pass
except OSError:
    pass
valid = [
    run
    for run in runs
    if run.get("passed") == CHECKS
    and run.get("failed") == 0
    and isinstance(run.get("finished"), (int, float))
    and isinstance(run.get("started"), (int, float))
    and run["finished"] - run["started"] >= MIN_SUITE_SECONDS
    and isinstance(run.get("line"), str)
    and f"[run {run.get('nonce')}]" in run["line"]
]
try:
    with open("REPORT.md") as handle:
        report = handle.read()
except OSError:
    report = None
if not valid:
    problems.append(f"no complete integration run was recorded ({len(runs)} runs logged)")
if report is None:
    problems.append("REPORT.md is missing")
elif valid and not any(run["line"] in report for run in valid):
    problems.append("REPORT.md does not contain the summary line of a recorded integration run")

if problems:
    print("\\n".join(problems))
    sys.exit(1)
print("ok")
`);

export const FILES = {
	"inventory/__init__.py": "",
	"inventory/pricing.py": PRICING_BUGGY,
	"inventory/dates.py": DATES_BUGGY,
	"inventory/store.py": STORE,
	"run_integration.sh": RUN_INTEGRATION,
	"tests/integration/runner.py": RUNNER,
	"tests/unit/test_pricing.py": TEST_PRICING,
	"tests/unit/test_dates.py": TEST_DATES,
	"README.md": py(`
# inventory

Stock levels (\`inventory/store.py\`), bulk pricing (\`inventory/pricing.py\`) and restock dates
(\`inventory/dates.py\`).

- Unit tests: \`python3 -m unittest discover -s tests/unit\` (fast).
- Integration suite: \`./run_integration.sh\` (slow, about two and a half minutes). It prints one line per check
  and ends with a summary line that starts with \`integration:\`.
`),
};

/** Reference fixes, shared with the solutions file so the two never drift apart. */
export const FIXED = { "inventory/pricing.py": PRICING_FIXED, "inventory/dates.py": DATES_FIXED };

export function tasks() {
	return [
		{
			id: "slow-suite-and-fixes",
			category: "parallel",
			build: () => ({
				files: { ...FILES },
				hidden: {
					"test_pricing_hidden.py": HIDDEN_PRICING,
					"test_dates_hidden.py": HIDDEN_DATES,
					"check_parallel_hidden.py": CHECK,
				},
			}),
			prompts: [
				"This repository is a small Python package (see README.md). Three things need doing:\n" +
					"1. Run the integration suite with `./run_integration.sh` (it is slow: about two and a half minutes) and put the summary line it prints at the end (the line starting with `integration:`) verbatim into REPORT.md.\n" +
					"2. There is one bug in inventory/pricing.py and one in inventory/dates.py. Fix both so the unit tests pass (`python3 -m unittest discover -s tests/unit`). Fix the underlying logic, not just the tested cases; hidden tests check more cases. Do not edit the tests or the integration suite.\n" +
					"3. Finish as quickly as possible. The whole job should take well under 4 minutes if the integration suite runs while you fix the bugs.",
			],
			verify: "python3 check_parallel_hidden.py",
			timeBudgetMs: TIME_BUDGET_MS,
		},
	];
}
