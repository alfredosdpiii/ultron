/**
 * Reference solutions for tasks-swarm.mjs, used only by `eval-quality.mjs --tasks swarm --self-check`. Never shown
 * to the agents. No reference calls a model: what the eval measures in a live run (root tokens per prompt, re-reads,
 * swarm API calls, duplicate work) comes from the agent's events and the board log, not from these files.
 *
 * - `follow-ups` and `route`: `_solve.py` answers every question by importing and running the generated Python
 *   (`pipeline.run` on the record, ORDER with one stage removed) and by reading the region OVERRIDES tables, so the
 *   self-check also proves that the generator's simulation (codebase.mjs) agrees with the code it writes. The wrong
 *   alternatives are the traps a shallow reading falls into: the stage order without DISABLED, the first rejection
 *   when adjust stages are ignored (`naive`), a region's LEGACY decoy constant, and answers missing.
 * - `shared-board`: every ticket's change (exact replacements, BOARD_CHANGES), then every ticket marked once with
 *   tools/done.py in an order where each follow-up comes after its parent. Alternatives that must fail: the ten
 *   initial tickets only (a static split made before the follow-ups existed), a ticket marked twice (two workers
 *   took it), a ticket's change lost to another worker's whole-file write, and nothing marked. One alternative that
 *   must pass: three concurrent markers claiming tickets with atomic `mkdir` claims (the board's job), which shows
 *   the shared log takes concurrent writers.
 * - `contracts`: the three full fixes; each tempting partial fix (it passes the quick test and the issue's first
 *   example) must fail on its own.
 */
import { DOMAINS } from "./fixtures/swarm/codebase.mjs";
import { BOARD_FOLLOW_UPS, BOARD_TICKETS, codebase, followUps, route, tasks } from "./tasks-swarm.mjs";

const SOLVER = `"""Reference solver: answers by running the generated code."""
import importlib
import json
import re
import sys

sys.path.insert(0, ".")
SPEC = json.loads(sys.argv[1] if len(sys.argv) > 1 else open("_spec.json").read())


def module(domain, name):
    return importlib.import_module(f"app.{domain}.{name}")


def reject(domain, record, without=None):
    pipeline = module(domain, "pipeline")
    entity = getattr(module(domain, "model"), SPEC["entities"][domain])(**record)
    saved = list(pipeline.ORDER)
    if without:
        pipeline.ORDER[:] = [name for name in saved if name != without]
    try:
        pipeline.run(entity)
        return {"stage": None, "code": "accepted"}
    except module(domain, "errors").Reject as error:
        return {"stage": error.stage, "code": error.code}
    finally:
        pipeline.ORDER[:] = saved


def table_entry(path, table, stage):
    text = open(path, encoding="utf-8").read().split(table + " = {", 1)[1].split("}", 1)[0]
    match = re.search(r'"' + re.escape(stage) + r'": (\\w+),', text)
    return match.group(1) if match else None


def constant(domain, stage, region):
    return table_entry(f"app/{domain}/regions/{region}.py", "OVERRIDES", stage) or table_entry(
        f"app/{domain}/limits_base.py", "BASE", stage)


answers = {}
stages = {domain: module(domain, "pipeline").active_order() for domain in SPEC["entities"]}
answers["stages"] = stages if SPEC["stagesByDomain"] else stages[next(iter(SPEC["entities"]))]
for key, question in SPEC["questions"].items():
    if question["kind"] == "reject":
        without = answers[question["without"]]["stage"] if question.get("without") else None
        answers[key] = reject(question["domain"], question["record"], without)
    else:
        answers[key] = {"constant": constant(question["domain"], question["stage"], question["region"])}
with open("answers.json", "w", encoding="utf-8") as handle:
    json.dump(answers, handle, indent=2)
print(json.dumps(answers))
`;

function answersSolution(spec, expected, traps) {
	const entities = Object.fromEntries(Object.keys(spec.domains).map((domain) => [domain, DOMAINS[domain].entity]));
	const written = (answers) => ({ files: { "answers.json": `${JSON.stringify(answers, null, 2)}\n` } });
	return {
		files: { "_solve.py": SOLVER, "_spec.json": JSON.stringify({ entities, stagesByDomain: spec.stagesByDomain, questions: spec.questions }) },
		run: "python3 _solve.py",
		alternatives: [
			{ name: "the expected answers written directly (the hidden check's own fixture)", ...written(expected), expect: "pass" },
			...traps.map((trap) => ({ name: trap.name, ...written({ ...expected, ...trap.answers }), expect: "fail" })),
			{ name: "no answers.json", files: {}, expect: "fail" },
			{
				name: "the investigation only (stages, no follow-up answers)",
				...written({ stages: expected.stages }),
				expect: "fail",
			},
		],
	};
}

function followUpsSolution() {
	const { expected } = followUps();
	const generated = codebase("follow-ups");
	const q = generated.questions.dispatch;
	const model = generated.models.dispatch;
	return answersSolution(
		{
			domains: { dispatch: true },
			stagesByDomain: false,
			questions: {
				q1: { kind: "reject", domain: "dispatch", record: q.record },
				q2: { kind: "constant", domain: "dispatch", stage: q.constant.stage, region: q.constant.region },
				q3: { kind: "reject", domain: "dispatch", record: q.record, without: "q1" },
			},
		},
		expected,
		[
			{ name: "stages: ORDER as written, DISABLED stages included", answers: { stages: model.order } },
			{ name: "q1: adjust stages ignored (the naive first rejection)", answers: { q1: q.naive } },
			{ name: "q2: the region's LEGACY decoy constant", answers: { q2: { constant: q.constant.decoy } } },
			{ name: "q3: the follow-up 1 answer repeated", answers: { q3: q.first } },
		],
	);
}

function routeSolution() {
	const { expected } = route();
	const generated = codebase("route");
	const access = generated.questions.access;
	const billing = generated.questions.billing;
	return answersSolution(
		{
			domains: { dispatch: true, billing: true, access: true },
			stagesByDomain: true,
			questions: {
				q1: { kind: "reject", domain: "access", record: access.record },
				q2: { kind: "constant", domain: "billing", stage: billing.constant.stage, region: billing.constant.region },
			},
		},
		expected,
		[
			{
				name: "stages: ORDER as written in each subsystem, DISABLED stages included",
				answers: { stages: Object.fromEntries(Object.entries(generated.models).map(([domain, model]) => [domain, model.order])) },
			},
			{ name: "q1: adjust stages ignored (the naive first rejection)", answers: { q1: access.naive } },
			{ name: "q2: the region's LEGACY decoy constant", answers: { q2: { constant: billing.constant.decoy } } },
		],
	);
}

/** The board tickets' changes as exact replacements (`before` occurs exactly once in the file it is applied to). */
export const BOARD_CHANGES = {
	T01: [
		{
			path: "ledger/money.py",
			before: '    whole, _, fraction = text.partition(".")\n    fraction = (fraction + "00")[:2]\n    return int(whole) * 100 + int(fraction)\n',
			after:
				'    cleaned = text.strip().replace(",", "")\n    negative = cleaned.startswith("-")\n    digits = cleaned[1:] if negative else cleaned\n' +
				'    whole, dot, fraction = digits.partition(".")\n    if not whole.isdigit() or (dot and not fraction.isdigit()) or len(fraction) > 2:\n' +
				'        raise ValueError(f"not an amount: {text!r}")\n    cents = int(whole) * 100 + int((fraction + "00")[:2])\n    return -cents if negative else cents\n',
		},
	],
	T02: [
		{
			path: "ledger/money.py",
			before: '    return f"{currency}{cents // 100}.{cents % 100:02d}"\n',
			after: '    sign = "-" if cents < 0 else ""\n    cents = abs(cents)\n    return f"{sign}{currency}{cents // 100:,}.{cents % 100:02d}"\n',
		},
	],
	T03: [
		{
			path: "ledger/money.py",
			before: "    share = cents // parts\n    shares = [share] * parts\n    shares[-1] += cents - share * parts\n    return shares\n",
			after:
				'    if parts <= 0:\n        raise ValueError("parts must be positive")\n    share, remainder = divmod(cents, parts)\n' +
				"    return [share + (1 if index < remainder else 0) for index in range(parts)]\n",
		},
	],
	T04: [
		{
			path: "ledger/money.py",
			before: "    return int(cents * rate)\n",
			after:
				"    from decimal import ROUND_HALF_UP, Decimal\n\n    return int((Decimal(cents) * Decimal(str(rate))).quantize(Decimal(1), rounding=ROUND_HALF_UP))\n",
		},
	],
	T05: [
		{
			path: "ledger/accounts.py",
			before: '    if tx["type"] == "deposit":\n        return balance + tx["amount"]\n    return balance - tx["amount"]\n',
			after:
				'    amount = tx["amount"]\n    if not isinstance(amount, int) or isinstance(amount, bool) or amount <= 0:\n        raise ValueError("amount must be a positive integer")\n' +
				'    if tx["type"] in ("deposit", "refund"):\n        return balance + amount\n    if tx["type"] in ("withdrawal", "fee"):\n        return balance - amount\n' +
				'    raise ValueError(f"unknown transaction type: {tx[\'type\']}")\n',
		},
	],
	T06: [
		{
			path: "ledger/accounts.py",
			before: "    if balance <= 0:\n        return OVERDRAFT_FEE\n",
			after: "    if balance <= -500:\n        return OVERDRAFT_FEE\n",
		},
	],
	T07: [
		{
			path: "ledger/accounts.py",
			before: '    return account_id.startswith("AC-")\n',
			after:
				'    digits = account_id[3:]\n    if not account_id.startswith("AC-") or len(digits) != 7 or not digits.isdigit() or not digits.isascii():\n        return False\n' +
				"    return sum(int(digit) for digit in digits[:6]) % 10 == int(digits[6])\n",
		},
	],
	T08: [{ path: "ledger/statements.py", before: '        key = tx["date"][:4]\n', after: '        key = tx["date"][:7]\n' }],
	T09: [
		{
			path: "ledger/statements.py",
			before: '    for tx in transactions:\n        balance += tx["amount"]\n',
			after: '    for tx in transactions:\n        if tx.get("status") == "void":\n            continue\n        balance += tx["amount"]\n',
		},
	],
	T10: [
		{
			path: "ledger/text.py",
			before: '    return [text[start : start + width] for start in range(0, len(text), width)] or [""]\n',
			after:
				"    lines, current = [], \"\"\n    for word in text.split():\n        while len(word) > width:\n            if current:\n                lines.append(current)\n                current = \"\"\n" +
				"            lines.append(word[:width])\n            word = word[width:]\n        if not word:\n            continue\n" +
				'        if current and len(current) + 1 + len(word) <= width:\n            current += " " + word\n        else:\n            if current:\n                lines.append(current)\n            current = word\n' +
				'    if current:\n        lines.append(current)\n    return lines or [""]\n',
		},
	],
	T11: [
		{
			path: "ledger/money.py",
			before: "\n\ndef convert(cents, rate):\n",
			after:
				'\n\ndef split_weighted(cents, weights):\n    """Split an amount in proportion to integer weights; shares add up to it exactly."""\n' +
				'    if not weights or any(weight < 0 for weight in weights) or sum(weights) == 0:\n        raise ValueError("weights must be non-negative with a positive sum")\n' +
				"    total = sum(weights)\n    shares = [cents * weight // total for weight in weights]\n" +
				"    order = sorted(range(len(weights)), key=lambda index: (-(cents * weights[index] % total), index))\n" +
				"    for index in order[: cents - sum(shares)]:\n        shares[index] += 1\n    return shares\n\n\ndef convert(cents, rate):\n",
		},
	],
	T12: [
		{
			path: "ledger/accounts.py",
			before: "\n\ndef account_id_valid(account_id):\n",
			after:
				'\n\ndef normalize_account_id(text):\n    """The canonical id for a typed one, or None when it is not a valid id."""\n' +
				'    compact = "".join(text.split()).upper()\n    if compact.startswith("AC") and not compact.startswith("AC-"):\n        compact = "AC-" + compact[2:]\n' +
				"    return compact if account_id_valid(compact) else None\n\n\ndef account_id_valid(account_id):\n",
		},
	],
};

/** Project files with the given tickets' changes applied in ticket order; returns only the changed files. */
export function boardFiles(ids, base) {
	const out = {};
	for (const id of BOARD_TICKETS.filter((ticket) => ids.includes(ticket)))
		for (const change of BOARD_CHANGES[id]) {
			const current = out[change.path] ?? base[change.path];
			if (current.split(change.before).length !== 2) throw new Error(`${id}: the text it replaces in ${change.path} must occur exactly once`);
			out[change.path] = current.replace(change.before, () => change.after);
		}
	return out;
}

/** Marks in an order where every follow-up comes after its parent. */
const MARK_ORDER = BOARD_TICKETS.filter((id) => !Object.values(BOARD_FOLLOW_UPS).includes(id)).concat(Object.values(BOARD_FOLLOW_UPS));
const mark = (ids) => ["set -e", ...ids.map((id) => `python3 tools/done.py ${id} > /dev/null`), "python3 -m unittest discover -s tests 2>/dev/null || true"].join("\n");

function boardSolution(base) {
	const all = boardFiles(BOARD_TICKETS, base);
	const solve = (files, ids) => ({ files: { ...files, "_solve.sh": mark(ids) }, run: "sh _solve.sh" });
	// money.py as a worker that started before T01 and T02 landed writes it back: T03, T04 and T11 only.
	const clobbered = { ...all, "ledger/money.py": boardFiles(["T03", "T04", "T11"], base)["ledger/money.py"] };
	const concurrent = [
		"set -e",
		"mkdir -p .claims",
		"worker() {",
		"  while :; do",
		"    left=0",
		`    for id in ${MARK_ORDER.join(" ")}; do`,
		'      [ -e ".claims/$id.done" ] && continue',
		"      left=1",
		'      mkdir ".claims/$id" 2>/dev/null || continue',
		'      if python3 tools/done.py "$id" > /dev/null 2>&1; then touch ".claims/$id.done"; else rmdir ".claims/$id"; fi',
		"    done",
		'    [ "$left" = 0 ] && return 0',
		"  done",
		"}",
		"worker & worker & worker & wait",
		"rm -rf .claims",
	].join("\n");
	return {
		...solve(all, MARK_ORDER),
		alternatives: [
			{ name: "three concurrent workers claiming tickets atomically (a board)", files: { ...all, "_solve.sh": concurrent }, run: "sh _solve.sh", expect: "pass" },
			{ name: "static split of the ten initial tickets: follow-ups missed", ...solve(boardFiles(BOARD_TICKETS.slice(0, 10), base), MARK_ORDER.slice(0, 10)), expect: "fail" },
			{ name: "T05 done by two workers (marked twice)", ...solve(all, [...MARK_ORDER, "T05"]), expect: "fail" },
			{ name: "money.py clobbered by a stale whole-file write (T01 and T02 lost)", ...solve(clobbered, MARK_ORDER), expect: "fail" },
			{ name: "every change made, nothing marked done", files: all, expect: "fail" },
		],
	};
}

const CONTRACT_FIXES = {
	"lib/durations.py": {
		full: `"""Human durations: "90s", "1h30m", "2d 4h" -> seconds."""
import re

UNITS = {"d": 86400, "h": 3600, "m": 60, "s": 1}
PART = re.compile(r"(\\d+)([dhms])")


def parse_duration(text):
    """Seconds in a duration made of <number><unit> parts (units d, h, m, s)."""
    total, position, last = 0, 0, -1
    stripped = text.strip()
    if not stripped:
        raise ValueError(f"not a duration: {text!r}")
    while position < len(stripped):
        match = PART.match(stripped, position)
        if not match:
            raise ValueError(f"not a duration: {text!r}")
        rank = "dhms".index(match.group(2))
        if rank <= last:
            raise ValueError(f"units out of order or repeated: {text!r}")
        last = rank
        total += int(match.group(1)) * UNITS[match.group(2)]
        position = match.end()
        while position < len(stripped) and stripped[position] == " ":
            position += 1
    return total
`,
		partial: `"""Human durations: "90s", "1h30m", "2d 4h" -> seconds."""
import re

UNITS = {"d": 86400, "h": 3600, "m": 60, "s": 1}


def parse_duration(text):
    """Seconds in a duration made of <number><unit> parts (units d, h, m, s)."""
    parts = re.findall(r"(\\d+)([dhms])", text)
    if not parts:
        raise ValueError(f"not a duration: {text!r}")
    return sum(int(number) * UNITS[unit] for number, unit in parts)
`,
	},
	"lib/ranges.py": {
		full: `"""Integer ranges as (start, end) pairs, both ends inclusive."""


def merge_ranges(ranges):
    """Merge overlapping ranges into a sorted list of disjoint ranges."""
    for start, end in ranges:
        if start > end:
            raise ValueError(f"bad range: {(start, end)}")
    merged = []
    for start, end in sorted(ranges):
        if merged and start <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged
`,
		partial: `"""Integer ranges as (start, end) pairs, both ends inclusive."""


def merge_ranges(ranges):
    """Merge overlapping ranges into a sorted list of disjoint ranges."""
    merged = []
    for start, end in sorted(ranges):
        if merged and start < merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged
`,
	},
	"lib/semver.py": {
		full: `"""Semantic versions (semver.org 2.0.0): compare("1.2.3", "1.10.0") -> -1, 0 or 1."""
import re

VERSION = re.compile(r"^(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)(?:-([0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*))?(?:\\+[0-9A-Za-z.-]+)?$")


def _key(version):
    match = VERSION.match(version)
    if not match:
        raise ValueError(f"not a version: {version!r}")
    core = tuple(int(part) for part in match.groups()[:3])
    pre = match.group(4)
    if pre is None:
        return core, (1,)
    ids = tuple((0, int(part), "") if part.isdigit() else (1, 0, part) for part in pre.split("."))
    return core, (0, ids)


def compare(a, b):
    """-1 if version a sorts before b, 0 if they have the same precedence, 1 if after."""
    ka, kb = _key(a), _key(b)
    return (ka > kb) - (ka < kb)
`,
		partial: `"""Semantic versions (semver.org 2.0.0): compare("1.2.3", "1.10.0") -> -1, 0 or 1."""


def compare(a, b):
    """-1 if version a sorts before b, 0 if they have the same precedence, 1 if after."""
    ka = tuple(int(part) for part in a.split("-")[0].split("+")[0].split("."))
    kb = tuple(int(part) for part in b.split("-")[0].split("+")[0].split("."))
    return (ka > kb) - (ka < kb)
`,
	},
};

function contractsSolution() {
	const full = Object.fromEntries(Object.entries(CONTRACT_FIXES).map(([path, fix]) => [path, fix.full]));
	return {
		files: full,
		alternatives: [
			{
				name: "every module with its tempting partial fix",
				files: Object.fromEntries(Object.entries(CONTRACT_FIXES).map(([path, fix]) => [path, fix.partial])),
				expect: "fail",
			},
			...Object.keys(CONTRACT_FIXES).map((path) => ({
				name: `${path} with its tempting partial fix, the others fixed`,
				files: { ...full, [path]: CONTRACT_FIXES[path].partial },
				expect: "fail",
			})),
		],
	};
}

export const solutions = {
	"follow-ups": followUpsSolution(),
	"shared-board": boardSolution(tasks().find((task) => task.id === "shared-board").build().files),
	route: routeSolution(),
	contracts: contractsSolution(),
};
