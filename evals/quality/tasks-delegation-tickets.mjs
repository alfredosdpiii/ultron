/**
 * `twelve-tickets`, the third DELEGATION task (tasks-delegation.mjs lists it after `six-services-deep`): twelve small
 * tickets in ONE shared Python project, several of them in the same files, in a git repository.
 *
 * Why a third task: the first two give every work stream its own directory, so concurrent workers never touch the
 * same file. Real backlogs are not like that: tickets land in the same modules. This task measures whether
 * concurrent work on one checkout still pays off, and what it costs when it goes wrong:
 *
 * - One package, shop/ (pricing, inventory, orders, text, report), with a README, quick tests and twelve tickets
 *   under tickets/ (T01.md ... T12.md), each a small feature or bug fix with acceptance criteria and the function it
 *   concerns. A ticket is a few turns of work (read the ticket and the function, edit, run the quick tests).
 * - Tickets share files but not functions: four in pricing.py, three in orders.py, two each in inventory.py and
 *   text.py, one in report.py. Workers that each read a whole file, work for a while and write the whole file back
 *   clobber each other (the last writer wins, the others' tickets are lost). Workers in their own git worktrees,
 *   merged back one after another, do not: every ticket's change sits in its own function, separated from its
 *   neighbours by unchanged lines, so git's 3-way merge combines them without conflicts
 *   (scripts/eval-delegation-tickets.test.mjs checks this with real git).
 * - Exactly one COUPLED pair: T07 adds a `coupon` argument to orders.order_total, and T08 ("builds on T07") extends
 *   that same coupon handling. One worker doing T07 then T08 has an easy job; two workers doing them from the same
 *   base both rewrite the same lines of order_total, and merging the second conflicts (the `split-coupled-pair`
 *   alternative in tasks-delegation-solutions.mjs shows it). The ticket text makes the dependency discoverable;
 *   the prompt names no strategy.
 * - The project is a git repository with one commit when the agent starts (`git: true`: scripts/eval-quality.mjs
 *   runs `git init` and commits the task files before the agent or a self-check reference sees them). The hidden
 *   check reads the working tree, so uncommitted changes count and nothing needs committing.
 *
 * Pass/fail is correctness only: the hidden check runs each ticket's hidden acceptance tests in its own interpreter
 * and passes only when all twelve pass; a ticket whose change got lost fails its own tests. It prints one line per
 * ticket and a JSON line `{"passed": k, "total": 12, "tickets": {...}}`, recorded as `metrics`. `timeBudgetMs`
 * (300 s) gives `withinBudget` and never gates. Calibration: tasks-delegation-solutions.mjs.
 *
 * The task files are a committed fixture (fixtures/delegation-tickets/project and /hidden), pinned by one sha256.
 * The reference changes are TICKETS below, exact replacements (`before` occurs exactly once in the file it is
 * applied to) so any subset of tickets gives valid files. Do not edit after measurements exist: add a new id instead.
 */
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { fixtureDigest, readTree } from "./fixture-tree.mjs";

/** Reported per run; 5 minutes, as the prompt says. */
export const TIME_BUDGET_MS = 300_000;

export const TICKET_IDS = ["T01", "T02", "T03", "T04", "T05", "T06", "T07", "T08", "T09", "T10", "T11", "T12"];

/** The coupled pair: the second builds on the first, in the same function. */
export const COUPLED = ["T07", "T08"];

/**
 * The independent units of work: every ticket on its own, except the coupled pair, which is one unit done in order.
 * This is how the tickets should be handed out (one worker per unit).
 */
export const UNITS = TICKET_IDS.filter((id) => id !== COUPLED[1]).map((id) => (id === COUPLED[0] ? [...COUPLED] : [id]));

const FIXTURE = fileURLToPath(new URL("./fixtures/delegation-tickets/", import.meta.url));
/** sha256 over every fixture file (path and content, sorted by path); a changed fixture refuses to load. */
const FIXTURE_SHA256 = "7302b28e0b80898994d0eab6325ae90476e7e91eaf6c8475420f13a80ebcb913";

function loadFixture() {
	const project = readTree(join(FIXTURE, "project"));
	const hidden = readTree(join(FIXTURE, "hidden"));
	const digest = fixtureDigest(project, hidden);
	if (digest !== FIXTURE_SHA256)
		throw new Error(`fixtures/delegation-tickets changed: sha256 ${digest}, frozen ${FIXTURE_SHA256}`);
	return { project, hidden };
}

const { project: FILES_RAW, hidden: HIDDEN_RAW } = loadFixture();
export const FILES = FILES_RAW;
export const HIDDEN = HIDDEN_RAW;

/**
 * Each ticket's change as exact replacements (`before` occurs exactly once in `path`; `fn` is the function it lies
 * in). T08's `before` is T07's `after`: it applies only on top of T07.
 */
export const TICKETS = {
	T01: [
		{
			path: "shop/pricing.py",
			fn: "apply_discount",
			before: "    return price_cents * (100 - percent) // 100\n",
			after: "    return (price_cents * (100 - percent) + 50) // 100\n",
		},
	],
	T02: [
		{
			path: "shop/pricing.py",
			fn: "tax_for",
			before: "    rate = TAX_RATES[region]\n",
			after:
				'    key = region.strip().upper()\n    if key not in TAX_RATES:\n        raise ValueError(f"unknown tax region: {region}")\n    rate = TAX_RATES[key]\n',
		},
	],
	T03: [
		{
			path: "shop/pricing.py",
			fn: "bulk_price",
			before: "    if quantity >= 10:\n        return total * 90 // 100\n",
			after: "    if quantity >= 100:\n        return total * 85 // 100\n    if quantity >= 10:\n        return total * 90 // 100\n",
		},
	],
	T04: [
		{
			path: "shop/pricing.py",
			fn: "format_price",
			before: '    return f"{currency}{cents // 100}.{cents % 100:02d}"\n',
			after:
				'    sign = "-" if cents < 0 else ""\n    cents = abs(cents)\n    return f"{sign}{currency}{cents // 100:,}.{cents % 100:02d}"\n',
		},
	],
	T05: [
		{
			path: "shop/inventory.py",
			fn: "reserve",
			before: '    """Take `quantity` units of `sku` out of `stock`; returns the units left."""\n',
			after:
				'    """Take `quantity` units of `sku` out of `stock`; returns the units left."""\n    if quantity <= 0:\n        raise ValueError("quantity must be positive")\n',
		},
	],
	T06: [
		{
			path: "shop/inventory.py",
			fn: "low_stock",
			before: "    return sorted((sku for sku, level in stock.items() if level < threshold), key=lambda sku: stock[sku])\n",
			after: "    return sorted((sku for sku, level in stock.items() if level <= threshold), key=lambda sku: (stock[sku], sku))\n",
		},
	],
	T07: [
		{
			path: "shop/orders.py",
			fn: "order_total",
			before: "def order_total(lines, weight_grams):\n",
			after: "def order_total(lines, weight_grams, coupon=None):\n",
		},
		{
			path: "shop/orders.py",
			fn: "order_total",
			before: "    amount = subtotal(lines)\n    shipping = 0 if amount >= FREE_SHIPPING_FROM else shipping_cost(weight_grams)\n",
			after:
				'    amount = subtotal(lines)\n    if coupon == "SAVE10":\n        amount -= (amount + 5) // 10\n    shipping = 0 if amount >= FREE_SHIPPING_FROM else shipping_cost(weight_grams)\n',
		},
	],
	T08: [
		{
			path: "shop/orders.py",
			fn: "order_total",
			before:
				'    if coupon == "SAVE10":\n        amount -= (amount + 5) // 10\n    shipping = 0 if amount >= FREE_SHIPPING_FROM else shipping_cost(weight_grams)\n',
			after:
				'    code = None if coupon is None else coupon.strip().upper()\n    if code not in (None, "SAVE10", "FREESHIP"):\n        raise ValueError(f"unknown coupon: {coupon}")\n    if code == "SAVE10":\n        amount -= (amount + 5) // 10\n    free = code == "FREESHIP" or amount >= FREE_SHIPPING_FROM\n    shipping = 0 if free else shipping_cost(weight_grams)\n',
		},
	],
	T09: [
		{
			path: "shop/orders.py",
			fn: "shipping_cost",
			before: "    extra_kg = (weight_grams - 1000) // 1000\n",
			after: "    extra_kg = (weight_grams - 1000 + 999) // 1000\n",
		},
	],
	T10: [
		{
			path: "shop/text.py",
			fn: "slugify",
			before: '    return "".join(out)\n',
			after: '    return "-".join(part for part in "".join(out).split("-") if part)\n',
		},
	],
	T11: [
		{
			path: "shop/text.py",
			fn: "truncate",
			before: "    return text[:width]\n",
			after: '    return text[: width - 3].rstrip() + "..."\n',
		},
	],
	T12: [
		{
			path: "shop/report.py",
			fn: "top_sellers",
			before: "    ranked = sorted(totals.items(), key=lambda item: item[1])\n",
			after: "    ranked = sorted(totals.items(), key=lambda item: (-item[1], item[0]))\n",
		},
	],
};

/**
 * T08 done on its own from the untouched project, as a worker given only T08 would have to: it adds the coupon
 * argument itself, differently from T07. Merged next to T07's change it conflicts (both rewrite the same lines of
 * order_total). Used by the `split-coupled-pair` alternative.
 */
export const T08_ALONE = [
	{
		path: "shop/orders.py",
		fn: "order_total",
		before:
			"def order_total(lines, weight_grams):\n    \"\"\"What the customer pays: the subtotal plus shipping; shipping is free from FREE_SHIPPING_FROM cents.\"\"\"\n    amount = subtotal(lines)\n    shipping = 0 if amount >= FREE_SHIPPING_FROM else shipping_cost(weight_grams)\n",
		after:
			"def order_total(lines, weight_grams, coupon=None):\n    \"\"\"What the customer pays: the subtotal plus shipping; shipping is free from FREE_SHIPPING_FROM cents.\"\"\"\n    amount = subtotal(lines)\n    code = coupon.strip().upper() if coupon else None\n    if code is not None and code not in (\"SAVE10\", \"FREESHIP\"):\n        raise ValueError(f\"unknown coupon: {coupon}\")\n    if code == \"SAVE10\":\n        amount = amount - (amount + 5) // 10\n    shipping = 0 if code == \"FREESHIP\" or amount >= FREE_SHIPPING_FROM else shipping_cost(weight_grams)\n",
	},
];

/** Apply replacements to `base` (a {path: content} tree); returns only the changed files. */
export function applyChanges(changes, base = FILES) {
	const out = {};
	for (const change of changes) {
		const current = out[change.path] ?? base[change.path];
		if (current === undefined) throw new Error(`${change.path} is not in the fixture`);
		if (current.split(change.before).length !== 2)
			throw new Error(`${change.path}: the text a change replaces (in ${change.fn}) must occur exactly once`);
		out[change.path] = current.replace(change.before, () => change.after);
	}
	return out;
}

/**
 * Project files with the given tickets done (in ticket order), from the untouched fixture. Returns only the files
 * that changed. T08 without T07 throws: it builds on T07's code.
 */
export function ticketFiles(ids = TICKET_IDS) {
	const unknown = ids.filter((id) => !TICKETS[id]);
	if (unknown.length) throw new Error(`unknown tickets: ${unknown.join(", ")}`);
	return applyChanges(TICKET_IDS.filter((id) => ids.includes(id)).flatMap((id) => TICKETS[id]));
}

/** Every ticket done. */
export const FIXED = ticketFiles();

export const PROMPT =
	"This repository is a small Python package, shop/ (see README.md), with twelve tickets under tickets/ (T01.md ... T12.md), each a small feature or bug fix with acceptance criteria.\n" +
	"1. Implement all twelve tickets. Hidden tests check every ticket's acceptance criteria in more cases than the quick tests, so implement the behaviour, not just the examples. Do not edit the tickets or the tests.\n" +
	"2. The tickets are small and mostly independent, though several touch the same files; read each ticket before starting on it. Run the quick tests from the repository root: `python3 -m unittest discover -s tests`.\n" +
	"3. Finish as fast as possible: the whole job must be done within 5 minutes.";

export const TASK = {
	id: "twelve-tickets",
	category: "delegation",
	build: () => ({ files: { ...FILES }, hidden: { ...HIDDEN } }),
	/** The task files are committed to a fresh git repository before the agent starts (scripts/eval-quality.mjs). */
	git: true,
	prompts: [PROMPT],
	verify: "python3 check_tickets_hidden.py",
	verifyTimeoutMs: 180_000,
	timeBudgetMs: TIME_BUDGET_MS,
};
