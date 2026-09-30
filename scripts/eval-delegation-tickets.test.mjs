import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { fixtureDigest, readTree } from "../evals/quality/fixture-tree.mjs";
import { solutions, TICKETS_CALIBRATION } from "../evals/quality/tasks-delegation-solutions.mjs";
import { tasks } from "../evals/quality/tasks-delegation.mjs";
import {
	applyChanges,
	COUPLED,
	FILES,
	FIXED,
	T08_ALONE,
	TICKET_IDS,
	TICKETS,
	TIME_BUDGET_MS,
	ticketFiles,
	UNITS,
} from "../evals/quality/tasks-delegation-tickets.mjs";
import { initGitRepo, verifyMetrics } from "./eval-quality.mjs";

const task = tasks().find((entry) => entry.id === "twelve-tickets");

/** Words that would name a runtime feature or strategy (the set's list, plus the git mechanics this task is about). */
const STRATEGY_WORDS =
	/\b(ultron|rlm|pi|spawn\w*|sub-?agents?|agents?|delegat\w*|parallel\w*|concurren\w*|simultaneous\w*|child\w*|workers?|background|async\w*|threads?|fork\w*|jobs|split|at the same time|worktrees?|merg\w*|commit\w*|branch\w*|git)\b/i;

/** Commits and merges in tests: identity from the repo, no signing, no hooks. */
const GIT = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];

function git(cwd, ...args) {
	const run = spawnSync("git", [...GIT, ...args], { cwd, encoding: "utf8" });
	return { status: run.status, stdout: run.stdout, stderr: run.stderr };
}

function writeFiles(dir, files) {
	for (const [path, content] of Object.entries(files)) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		writeFileSync(join(dir, path), content);
	}
}

function project(extra = {}) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-delegation-tickets-test-"));
	const { files, hidden } = task.build();
	writeFiles(dir, { ...files, ...extra, ...hidden });
	return dir;
}

/** The hidden check's per-ticket outcome with the given changed files. */
function check(extra) {
	const dir = project(extra);
	try {
		const run = spawnSync("python3", ["check_tickets_hidden.py"], { cwd: dir, encoding: "utf8" });
		return { status: run.status, metrics: verifyMetrics(run.stdout), stdout: run.stdout };
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

const failing = (metrics) =>
	Object.entries(metrics.tickets)
		.filter(([, ok]) => !ok)
		.map(([id]) => id);

/** The top-level function a character offset of a Python file lies in (by the last `def` at column 0 before it). */
function functionAt(content, offset) {
	const defs = [...content.slice(0, offset + 1).matchAll(/^def (\w+)\(/gm)];
	return defs.at(-1)?.[1] ?? null;
}

test("twelve-tickets: in the delegation set, a git repository, an agent-neutral prompt and a 5 minute budget", () => {
	assert.ok(task, "twelve-tickets is in tasks()");
	assert.equal(task.category, "delegation");
	assert.equal(task.git, true);
	assert.equal(task.timeBudgetMs, 5 * 60 * 1000);
	assert.equal(TIME_BUDGET_MS, task.timeBudgetMs);
	for (const prompt of task.prompts) {
		assert.doesNotMatch(prompt, STRATEGY_WORDS);
		assert.match(prompt, /within 5 minutes/);
		assert.match(prompt, /independent/);
		assert.match(prompt, /tickets\//);
	}
	// Twelve ticket files, each naming the function it concerns; T08 says it builds on T07.
	assert.deepEqual(
		Object.keys(FILES)
			.filter((path) => path.startsWith("tickets/"))
			.sort(),
		TICKET_IDS.map((id) => `tickets/${id}.md`),
	);
	for (const id of TICKET_IDS) {
		const ticket = FILES[`tickets/${id}.md`];
		assert.match(ticket, new RegExp(`^# ${id}: `), id);
		assert.match(ticket, /## Acceptance criteria/, id);
		for (const change of TICKETS[id]) assert.ok(ticket.includes(`\`${change.path}\`, \`${change.fn}\``), `${id}: names ${change.fn}`);
	}
	assert.match(FILES[`tickets/${COUPLED[1]}.md`], new RegExp(`builds on ${COUPLED[0]}`));
	// Nothing in the project names a strategy either.
	for (const path of ["README.md", ...TICKET_IDS.map((id) => `tickets/${id}.md`)]) assert.doesNotMatch(FILES[path], STRATEGY_WORDS, path);
});

test("twelve-tickets: the fixture is pinned by its sha256", () => {
	const source = readFileSync(fileURLToPath(new URL("../evals/quality/tasks-delegation-tickets.mjs", import.meta.url)), "utf8");
	const pinned = source.match(/const FIXTURE_SHA256 = "([0-9a-f]{64})";/)?.[1];
	assert.ok(pinned, "a sha256 is pinned");
	const fixture = fileURLToPath(new URL("../evals/quality/fixtures/delegation-tickets/", import.meta.url));
	assert.equal(fixtureDigest(readTree(join(fixture, "project")), readTree(join(fixture, "hidden"))), pinned);
});

test("twelve-tickets: tickets share files but not functions, except the coupled pair", () => {
	const byFile = {};
	for (const id of TICKET_IDS) for (const change of TICKETS[id]) (byFile[change.path] ??= new Set()).add(id);
	const shared = Object.entries(byFile).filter(([, ids]) => ids.size >= 2);
	assert.ok(shared.length >= 3, `files touched by two or more tickets: ${shared.map(([path]) => path).join(", ")}`);
	assert.ok(byFile["shop/pricing.py"].size >= 4);
	// Every change lies in the function it names, in the file it applies to (T08's: on top of T07's).
	for (const id of TICKET_IDS) {
		const base = id === COUPLED[1] ? { ...FILES, ...ticketFiles([COUPLED[0]]) } : FILES;
		const current = { ...base };
		for (const change of TICKETS[id]) {
			const offset = current[change.path].indexOf(change.before);
			assert.notEqual(offset, -1, `${id}: ${change.fn}`);
			assert.equal(functionAt(current[change.path], offset + change.before.length - 1), change.fn, `${id} in ${change.fn}`);
			Object.assign(current, applyChanges([change], current));
		}
	}
	// Apart from the pair, no two tickets touch the same function.
	const owner = {};
	for (const id of TICKET_IDS.filter((ticket) => !COUPLED.includes(ticket)))
		for (const change of TICKETS[id]) {
			const key = `${change.path}:${change.fn}`;
			assert.ok(!owner[key] || owner[key] === id, `${key}: ${owner[key]} and ${id}`);
			owner[key] = id;
		}
	// The coupled pair: the same function, and the second only applies on top of the first.
	const [first, second] = COUPLED;
	assert.deepEqual([...new Set(TICKETS[first].map((change) => `${change.path}:${change.fn}`))], ["shop/orders.py:order_total"]);
	assert.deepEqual([...new Set(TICKETS[second].map((change) => `${change.path}:${change.fn}`))], ["shop/orders.py:order_total"]);
	assert.throws(() => ticketFiles([second]), /must occur exactly once/);
	assert.ok(!Object.keys(owner).includes("shop/orders.py:order_total"));
	// The units hand out every ticket once, the pair together.
	assert.deepEqual(UNITS.flat(), TICKET_IDS);
	assert.deepEqual(
		UNITS.filter((unit) => unit.length > 1),
		[COUPLED],
	);
});

test("twelve-tickets: the hidden check needs all twelve tickets and reports each one", { timeout: 180_000 }, () => {
	const untouched = check({});
	assert.notEqual(untouched.status, 0);
	assert.deepEqual(untouched.metrics, { passed: 0, total: 12, tickets: Object.fromEntries(TICKET_IDS.map((id) => [id, false])) });
	const solved = check(FIXED);
	assert.equal(solved.status, 0, solved.stdout);
	assert.equal(solved.metrics.passed, 12);
	// Each ticket left undone fails exactly that ticket (T07 undone takes T08 with it).
	for (const id of TICKET_IDS) {
		const undone = id === COUPLED[0] ? COUPLED : [id];
		const outcome = check(ticketFiles(TICKET_IDS.filter((other) => !undone.includes(other))));
		assert.notEqual(outcome.status, 0, id);
		assert.deepEqual(failing(outcome.metrics), undone, id);
	}
	// Each unit done alone on the untouched project passes exactly its own tickets: no ticket needs another unit.
	for (const unit of UNITS) {
		const outcome = check(ticketFiles(unit));
		assert.deepEqual(failing(outcome.metrics), TICKET_IDS.filter((id) => !unit.includes(id)), unit.join("+"));
	}
	// T08 done alone (its worker adding the coupon argument itself) is a correct implementation of both tickets:
	// the split pair fails only because it does not merge.
	const others = ticketFiles(TICKET_IDS.filter((id) => !COUPLED.includes(id)));
	assert.equal(check({ ...others, ...applyChanges(T08_ALONE, { ...FILES, ...others }) }).metrics.passed, 12);
});

test("twelve-tickets: the quick tests pass on the untouched project and with every ticket done", { timeout: 60_000 }, () => {
	for (const extra of [{}, FIXED]) {
		const dir = project(extra);
		try {
			const run = spawnSync("python3", ["-B", "-m", "unittest", "discover", "-s", "tests"], { cwd: dir, encoding: "utf8" });
			assert.equal(run.status, 0, run.stderr);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	}
});

test("initGitRepo commits the task files once with a local identity; later files stay untracked", async () => {
	const dir = mkdtempSync(join(tmpdir(), "ultron-delegation-tickets-git-"));
	try {
		const { files, hidden } = task.build();
		writeFiles(dir, files);
		await initGitRepo(dir);
		writeFiles(dir, hidden);
		assert.equal(git(dir, "rev-list", "--count", "HEAD").stdout.trim(), "1");
		assert.equal(git(dir, "log", "-1", "--format=%an <%ae> %s").stdout.trim(), "Eval <eval@localhost> Initial commit");
		assert.deepEqual(git(dir, "ls-files").stdout.trim().split("\n").sort(), Object.keys(files).sort());
		// Only the hidden files are new; no task file is modified.
		const status = git(dir, "status", "--porcelain", "--untracked-files=all").stdout.trim().split("\n");
		assert.ok(status.every((line) => line.startsWith("?? ")), status.join("\n"));
		assert.deepEqual(status.map((line) => line.slice(3)).sort(), Object.keys(hidden).sort());
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

/**
 * A repository of the untouched project with one worktree and branch per entry of `work` ({name: changed files},
 * committed there), merged back into the main tree one after another. Returns each merge's outcome and the files.
 */
async function worktreeMerge(work) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-delegation-tickets-merge-"));
	const trees = mkdtempSync(join(tmpdir(), "ultron-delegation-tickets-wt-"));
	try {
		writeFiles(dir, task.build().files);
		await initGitRepo(dir);
		for (const [name, files] of Object.entries(work)) {
			assert.equal(git(dir, "worktree", "add", "-q", "-b", name, join(trees, name), "HEAD").status, 0);
			writeFiles(join(trees, name), files);
			assert.equal(git(join(trees, name), "commit", "-q", "-a", "-m", name).status, 0);
		}
		const merges = {};
		for (const name of Object.keys(work)) {
			const merge = git(dir, "merge", "--no-ff", "--no-edit", name);
			merges[name] = merge.status === 0;
			if (!merges[name]) git(dir, "merge", "--abort");
		}
		const result = Object.fromEntries(Object.keys(FILES).map((path) => [path, readFileSync(join(dir, path), "utf8")]));
		return { merges, result };
	} finally {
		rmSync(dir, { recursive: true, force: true });
		rmSync(trees, { recursive: true, force: true });
	}
}

test("twelve-tickets: per-unit worktrees merge back with real git without conflicts", { timeout: 60_000 }, async () => {
	const { merges, result } = await worktreeMerge(Object.fromEntries(UNITS.map((unit) => [unit.join("-"), ticketFiles(unit)])));
	assert.ok(Object.values(merges).every(Boolean), JSON.stringify(merges));
	assert.deepEqual(result, { ...FILES, ...FIXED });
});

test("twelve-tickets: the coupled pair done from the same base conflicts on merge", { timeout: 60_000 }, async () => {
	const [first, second] = COUPLED;
	const { merges, result } = await worktreeMerge({ [first]: ticketFiles([first]), [second]: applyChanges(T08_ALONE) });
	assert.deepEqual(merges, { [first]: true, [second]: false });
	// The aborted merge leaves the main tree as the first ticket left it.
	assert.equal(result["shop/orders.py"], ticketFiles([first])["shop/orders.py"]);
});

test("twelve-tickets: the budget separates per-unit concurrency from one ticket after another", () => {
	const { turnSeconds, turnsPerTicket, orchestrationTurns } = TICKETS_CALIBRATION;
	const ticket = turnSeconds * turnsPerTicket;
	const around = turnSeconds * orchestrationTurns;
	const longestUnit = Math.max(...UNITS.map((unit) => unit.length)) * ticket;
	assert.ok((longestUnit + around) * 1000 < TIME_BUDGET_MS / 2, "parallel reference");
	// The slow end (15 s x 7 turns per ticket) still fits with one unit per worker.
	assert.ok((Math.max(...UNITS.map((unit) => unit.length)) * 15 * 7 + 2 * 15) * 1000 < TIME_BUDGET_MS);
	assert.ok((TICKET_IDS.length * ticket + around) * 1000 > TIME_BUDGET_MS * 1.5, "sequential");
	const solution = solutions[task.id];
	assert.equal(solution.expectWithinBudget, true);
	assert.deepEqual(
		solution.alternatives.filter((entry) => entry.expectWithinBudget !== undefined).map((entry) => [entry.expect, entry.expectWithinBudget]),
		[["pass", false]],
	);
	const names = solution.alternatives.map((entry) => `${entry.expect} ${entry.name}`);
	for (const kind of ["pass sequential", "fail naive-shared-parallel", "fail split-coupled-pair"])
		assert.ok(names.some((name) => name.startsWith(kind)), kind);
	assert.equal(names.filter((name) => name.startsWith("fail all tickets done but")).length, 12);
	// The reference really uses git worktrees and merges them back; the naive alternative writes in the shared tree.
	assert.match(solution.files["_solve.sh"], /worktree add/);
	assert.match(solution.files["_solve.sh"], /merge -q --no-ff/);
	assert.doesNotMatch(solution.alternatives[1].files["_solve.sh"], /worktree/);
});
