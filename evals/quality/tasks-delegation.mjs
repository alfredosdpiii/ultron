/**
 * Frozen DELEGATION quality task: several independent work streams, each needing several model turns, under a
 * wall-clock budget that one sequential agent is unlikely to meet.
 *
 * `six-services` ships six small, unrelated Python packages under services/ (alpha ... zeta): a token-bucket rate
 * limiter, invoice totals with decimal rounding and discount allocation, reporting date windows, layered config
 * merging, an LRU cache with expiry, and customer CSV normalization. Each has its own SPEC.md, 3-5 source files, a
 * visible quick test file, and three bugs spread across its files (listed in BUGS below). The quick tests catch
 * some of the bugs; the rest are found by reading the SPEC. No fix transfers between services: they share no code
 * and no domain. A service is a few turns of work for a capable model (read the spec and code, reproduce, fix three
 * bugs in two or three files, rerun the tests); six of them in sequence is the whole job for one agent.
 *
 * Pass/fail stays correctness only: the hidden check runs every service's hidden suite (SPEC cases plus a seeded,
 * randomized comparison with a reference implementation, a second or two per service) and passes only when all six
 * pass. It prints one line per service and a JSON line `{"passed": k, "total": 6, "services": {...}}`, which the
 * eval records as `metrics`. Wall time is recorded beside it: `timeBudgetMs` gives each run `withinBudget` and never
 * gates. The prompt is agent-neutral: it says the services are independent and asks for speed within the budget,
 * and names no runtime feature. An agent that works on the services in parallel should finish well inside it; a
 * very fast sequential agent can too. See tasks-delegation-solutions.mjs for the budget calibration.
 *
 * The task files are a committed fixture (fixtures/delegation/project, and the hidden files in
 * fixtures/delegation/hidden), pinned by one sha256 over every file. The reference fixes are the BUGS replacements,
 * applied to the fixture here so the solutions file and the script tests share them.
 * Same contract as tasks-hard.mjs (`build()` returns `{ files, hidden }`).
 * Do not edit the task after measurements exist: add a new id instead (see FROZEN_AT).
 *
 * The set's second task, `six-services-deep` (tasks-delegation-deep.mjs), was added after gpt-6-sol fixed all six
 * services here from one batched read: its bugs are found only by running a slow, staged check harness and
 * iterating, and the hidden check needs a logged passing harness run on each service's final code.
 */
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { fixtureDigest, readTree } from "./fixture-tree.mjs";
import { TASK as DEEP_TASK } from "./tasks-delegation-deep.mjs";

export { fixtureDigest };

export const FROZEN_AT = "2026-09-27-delegation";

/** Reported per run; 5 minutes, as the prompt says. Calibration: tasks-delegation-solutions.mjs. */
export const TIME_BUDGET_MS = 300_000;

export const SERVICES = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];

const FIXTURE = fileURLToPath(new URL("./fixtures/delegation/", import.meta.url));
/** sha256 over every fixture file (path and content, sorted by path); a changed fixture refuses to load. */
const FIXTURE_SHA256 = "0760eed7560f099ee38d9c65e00424879e867b22d8d9c0f6458d1e5c9b4f8069";

function loadFixture() {
	const project = readTree(join(FIXTURE, "project"));
	const hidden = readTree(join(FIXTURE, "hidden"));
	const digest = fixtureDigest(project, hidden);
	if (digest !== FIXTURE_SHA256)
		throw new Error(`fixtures/delegation changed: sha256 ${digest}, frozen ${FIXTURE_SHA256}`);
	return { project, hidden };
}

const { project: FILES_RAW, hidden: HIDDEN_RAW } = loadFixture();
export const FILES = FILES_RAW;
export const HIDDEN = HIDDEN_RAW;

/**
 * The bugs, three per service, as exact replacements in the fixture (`buggy` occurs exactly once in `path`).
 * Order matters for the self-check: the first bug of each service is the one a partial fix repairs.
 */
export const BUGS = {
	alpha: [
		{
			path: "ratelimit/bucket.py",
			what: "refill is not capped at capacity",
			buggy: "self.tokens = self.tokens + elapsed * self.rate",
			fixed: "self.tokens = min(self.capacity, self.tokens + elapsed * self.rate)",
		},
		{
			path: "ratelimit/clock.py",
			what: "a backwards tick drains tokens and moves the bucket's clock back",
			buggy: "    return now - last, now\n",
			fixed: "    if now <= last:\n        return 0, last\n    return now - last, now\n",
		},
		{
			path: "ratelimit/limiter.py",
			what: "retry_after rounds the wait down instead of up (and must count ticks before the bucket's last tick)",
			buggy: "return (n - tokens) // self.rate",
			fixed: "return max(0, bucket.last - now) + -(-(n - tokens) // self.rate)",
		},
	],
	beta: [
		{
			path: "invoice/money.py",
			what: "rounds half to even instead of half up",
			buggy: "rounding=decimal.ROUND_HALF_EVEN",
			fixed: "rounding=decimal.ROUND_HALF_UP",
		},
		{
			path: "invoice/tax.py",
			what: "tax is charged before the line's discount",
			buggy: "round_cents(net * rate_percent / 100)",
			fixed: "round_cents((net - discount) * rate_percent / 100)",
		},
		{
			path: "invoice/discount.py",
			what: "leftover cents go to the smallest remainders first",
			buggy: "key=lambda i: exact[i] - shares[i])",
			fixed: "key=lambda i: (-(exact[i] - shares[i]), i))",
		},
	],
	gamma: [
		{
			path: "windows/window.py",
			what: "weeks start on Sunday instead of Monday",
			buggy: "timedelta(days=(anchor.weekday() + 1) % 7)",
			fixed: "timedelta(days=anchor.weekday())",
		},
		{
			path: "windows/months.py",
			what: "a result in December becomes month 0",
			buggy: "    month = month % 12\n",
			fixed: "    month = (month - 1) % 12 + 1\n",
		},
		{
			path: "windows/parse.py",
			what: "kind names are case-sensitive",
			buggy: "kind = text.strip()\n",
			fixed: "kind = text.strip().lower()\n",
		},
	],
	delta: [
		{
			path: "confmerge/env.py",
			what: "variable names are split on single underscores",
			buggy: '.split("_")]',
			fixed: '.split("__")]',
		},
		{
			path: "confmerge/merge.py",
			what: "merging into a nested dict modifies the base input",
			buggy: "_merge_into(current, value)",
			fixed: "result[key] = deep_merge(current, value)",
		},
		{
			path: "confmerge/types.py",
			what: "negative integers stay strings",
			buggy: '_INT = re.compile(r"^\\d+$")',
			fixed: '_INT = re.compile(r"^-?\\d+$")',
		},
	],
	epsilon: [
		{
			path: "ttlcache/entry.py",
			what: "an entry is still live at its expiry tick",
			buggy: "return now > self.expires_at",
			fixed: "return now >= self.expires_at",
		},
		{
			path: "ttlcache/store.py",
			what: "rewriting a key keeps its old recency",
			buggy: "        self._entries[key] = entry\n",
			fixed: "        self._entries[key] = entry\n        self._entries.move_to_end(key)\n",
		},
		{
			path: "ttlcache/cache.py",
			what: "a full cache evicts before removing expired entries",
			buggy: "            self._evict()\n",
			fixed: "            self._purge(now)\n            if len(self._store) >= self.capacity:\n                self._evict()\n",
		},
	],
	zeta: [
		{
			path: "csvnorm/dedupe.py",
			what: "the last duplicate is kept instead of the first",
			buggy: 'chosen[value if value else ("blank", index)] = index',
			fixed: 'chosen.setdefault(value if value else ("blank", index), index)',
		},
		{
			path: "csvnorm/fields.py",
			what: "accounting negatives in parentheses come out positive",
			buggy: "        text = text[1:-1].strip()\n",
			fixed: "        text = text[1:-1].strip()\n        negative = True\n",
		},
		{
			path: "csvnorm/reader.py",
			what: "a leading byte order mark ends up in the first header name",
			buggy: "def read_rows(text):\n",
			fixed: 'def read_rows(text):\n    if text.startswith("\\ufeff"):\n        text = text[1:]\n',
		},
	],
};

/**
 * Service files with the chosen bugs fixed: `fixes` maps a service to the indexes of its BUGS to repair (default:
 * all). Returns only the files that changed, keyed by project path.
 */
export function fixedFiles(fixes = Object.fromEntries(SERVICES.map((service) => [service, [0, 1, 2]]))) {
	const out = {};
	for (const [service, indexes] of Object.entries(fixes))
		for (const index of indexes) {
			const bug = BUGS[service][index];
			const path = `services/${service}/${bug.path}`;
			const current = out[path] ?? FILES[path];
			if (current === undefined) throw new Error(`${path} is not in the fixture`);
			if (current.split(bug.buggy).length !== 2) throw new Error(`${path}: the buggy text of "${bug.what}" must occur exactly once`);
			out[path] = current.replace(bug.buggy, () => bug.fixed);
		}
	return out;
}

/** Every bug of every service fixed. */
export const FIXED = fixedFiles();

export const PROMPT =
	"This repository holds six small Python services under services/ (alpha, beta, gamma, delta, epsilon, zeta; see README.md). " +
	"Each has its own SPEC.md, source package and quick tests, and each has several bugs spread across its files: its code does not do what its SPEC.md says.\n" +
	"1. Fix all six services so each one behaves exactly as its SPEC.md says. Hidden tests check every service against its SPEC.md in many more cases than the quick tests, so fix the underlying logic, not just the tested cases. Do not edit the SPEC.md files or the tests.\n" +
	"2. The services are independent: they share no code, and a fix in one never affects another. Run a service's quick tests from its own directory, e.g. `cd services/alpha && python3 -m unittest discover -s tests`.\n" +
	"3. Finish as fast as possible: the whole job must be done within 5 minutes.";

export function tasks() {
	return [
		{
			id: "six-services",
			category: "delegation",
			build: () => ({ files: { ...FILES }, hidden: { ...HIDDEN } }),
			prompts: [PROMPT],
			verify: "python3 check_delegation_hidden.py",
			verifyTimeoutMs: 180_000,
			timeBudgetMs: TIME_BUDGET_MS,
		},
		DEEP_TASK,
	];
}
