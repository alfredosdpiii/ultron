import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { AGENT_TURN_SECONDS, ORCHESTRATION_TURNS, solutions, TURNS_PER_SERVICE } from "../evals/quality/tasks-delegation-solutions.mjs";
import { BUGS, FILES, FIXED, fixedFiles, SERVICES, TIME_BUDGET_MS, tasks } from "../evals/quality/tasks-delegation.mjs";
import { childUptake, formatRunLine, isGatedComparison, summarizeUptake, verifyMetrics } from "./eval-quality.mjs";

const [task] = tasks();

function project(extra = {}) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-delegation-test-"));
	const { files, hidden } = task.build();
	for (const [path, content] of Object.entries({ ...files, ...extra, ...hidden })) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		writeFileSync(join(dir, path), content);
	}
	return dir;
}

/** The hidden check's per-service outcome for the given fixed files. */
function check(extra) {
	const dir = project(extra);
	try {
		const run = spawnSync("python3", ["check_delegation_hidden.py"], { cwd: dir, encoding: "utf8" });
		return { status: run.status, metrics: verifyMetrics(run.stdout), stdout: run.stdout };
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

test("the delegation set is one timed task over six distinct services with an agent-neutral prompt", () => {
	assert.equal(tasks().length, 1);
	assert.equal(task.id, "six-services");
	assert.equal(task.timeBudgetMs, TIME_BUDGET_MS);
	for (const prompt of task.prompts) {
		assert.doesNotMatch(prompt, /\b(ultron|rlm|spawn\w*|sub-?agents?|delegat\w*|parallel\w*|concurren\w*|child\w*|workers?)\b/i);
		assert.match(prompt, /within 5 minutes/);
		assert.match(prompt, /independent/);
	}
	assert.equal(TIME_BUDGET_MS, 5 * 60 * 1000);
	// Six services, each with a SPEC, 3-5 source files and a quick test file; three bugs in at least two files each.
	assert.deepEqual(SERVICES, ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]);
	const packages = new Set();
	for (const service of SERVICES) {
		const own = Object.keys(FILES).filter((path) => path.startsWith(`services/${service}/`));
		assert.ok(own.includes(`services/${service}/SPEC.md`), service);
		assert.ok(own.includes(`services/${service}/tests/test_quick.py`), service);
		const sources = own.filter((path) => path.endsWith(".py") && !path.includes("/tests/"));
		assert.ok(sources.length >= 3 && sources.length <= 5, `${service}: ${sources.length} source files`);
		packages.add(sources[0].split("/")[2]);
		assert.equal(BUGS[service].length, 3, service);
		assert.ok(new Set(BUGS[service].map((bug) => bug.path)).size >= 2, `${service}: bugs spread across files`);
	}
	assert.equal(packages.size, 6);
});

test("the budget separates six services worked in parallel from six in sequence", () => {
	const turn = AGENT_TURN_SECONDS * 1000;
	const parallel = (TURNS_PER_SERVICE + ORCHESTRATION_TURNS) * turn;
	const sequential = (SERVICES.length * TURNS_PER_SERVICE + ORCHESTRATION_TURNS) * turn;
	assert.ok(parallel < TIME_BUDGET_MS / 2, `parallel ${parallel} ms`);
	assert.ok(sequential > TIME_BUDGET_MS * 1.5, `sequential ${sequential} ms`);
	// At the slow end of measured turn times (15 s x 10 turns) the parallel pace still fits.
	assert.ok((10 + ORCHESTRATION_TURNS) * 15_000 < TIME_BUDGET_MS);
	const solution = solutions[task.id];
	assert.equal(solution.expectWithinBudget, true);
	const timed = solution.alternatives.filter((entry) => entry.expectWithinBudget !== undefined);
	assert.deepEqual(
		timed.map((entry) => [entry.expect, entry.expectWithinBudget]),
		[["pass", false]],
	);
	const wrong = solution.alternatives.filter((entry) => entry.expect === "fail").map((entry) => entry.name);
	assert.ok(wrong.includes("only the first bug fixed in every service"));
	assert.equal(wrong.filter((name) => name.startsWith("five services fixed")).length, 6);
	assert.equal(wrong.filter((name) => name.includes(": all fixed but")).length, 18);
});

test("the hidden check needs every bug of every service fixed and reports each service", { timeout: 120_000 }, () => {
	const untouched = check({});
	assert.notEqual(untouched.status, 0);
	assert.deepEqual(untouched.metrics, { passed: 0, total: 6, services: Object.fromEntries(SERVICES.map((s) => [s, false])) });
	const solved = check(FIXED);
	assert.equal(solved.status, 0, solved.stdout);
	assert.equal(solved.metrics.passed, 6);
	// Each bug left alone fails exactly its own service.
	for (const service of SERVICES)
		for (const index of [0, 1, 2]) {
			const fixes = Object.fromEntries(SERVICES.map((other) => [other, other === service ? [0, 1, 2].filter((i) => i !== index) : [0, 1, 2]]));
			const outcome = check(fixedFiles(fixes));
			assert.notEqual(outcome.status, 0, `${service} bug ${index}`);
			assert.equal(outcome.metrics.passed, 5, `${service} bug ${index}`);
			assert.equal(outcome.metrics.services[service], false, `${service} bug ${index}`);
		}
	// Only the first bug of each service fixed: nothing passes.
	assert.equal(check(fixedFiles(Object.fromEntries(SERVICES.map((s) => [s, [0]])))).metrics.passed, 0);
});

test("the untouched quick tests fail in every service and pass once it is fixed", { timeout: 60_000 }, () => {
	for (const [extra, expected] of [
		[{}, false],
		[FIXED, true],
	]) {
		const dir = project(extra);
		try {
			for (const service of SERVICES) {
				const run = spawnSync("python3", ["-B", "-m", "unittest", "discover", "-s", "tests"], { cwd: join(dir, "services", service) });
				assert.equal(run.status === 0, expected, `${service}: ${run.stderr}`);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	}
});

test("subagents are counted with their nesting depth and printed on the run line", () => {
	const tasksSeen = [
		{ id: "c1", definition: "rlm-child@1" },
		{ id: "c2", definition: "rlm-child@1" },
		{ id: "g1", definition: "rlm-child@1", parentId: "c1" },
		{ id: "f1", definition: "rlm-infer@1", parentId: "g1" },
		{ id: "j1", definition: "background-job@1" },
	];
	assert.deepEqual(childUptake(tasksSeen), { childrenSpawned: 3, childDepth: 2 });
	assert.deepEqual(childUptake([{ id: "f", definition: "rlm-map@1" }]), { childrenSpawned: 0, childDepth: 0 });
	// A parent chain that loops does not hang.
	assert.deepEqual(childUptake([{ id: "a", definition: "rlm-child@1", parentId: "a" }]), { childrenSpawned: 1, childDepth: 1 });
	const base = {
		task: "six-services",
		variant: "ultron",
		trial: 1,
		passed: true,
		durationMs: 150_000,
		timeBudgetMs: TIME_BUDGET_MS,
		withinBudget: true,
		toolsByName: { rlm: 9 },
		framesSpawned: 6,
		childrenSpawned: 6,
		childDepth: 1,
		metrics: { passed: 6, total: 6, services: {} },
	};
	assert.match(formatRunLine(base), /^TIME\s+150\.0s \/ budget 300\.0s WITHIN {2}PASS ultron six-services#1 {2}children 6 \(depth 1\) services 6\/6 tools \{"rlm":9\} frames 6$/);
	assert.match(formatRunLine({ ...base, childrenSpawned: 0, childDepth: 0 }), / children 0 services 6\/6 /);
	assert.match(
		formatRunLine({ ...base, variant: "pi", childrenSpawned: null, childDepth: null, framesSpawned: null, metrics: undefined, toolsByName: { bash: 3 } }),
		/ {2}children n\/a tools \{"bash":3\} frames n\/a$/,
	);
	assert.deepEqual(summarizeUptake([base, { ...base, childrenSpawned: 0 }, { ...base, childrenSpawned: null }]).childrenSpawned, {
		runs: 2,
		total: 6,
		median: 3,
	});
});

test("the delegation set is never read by the release gate", () => {
	const gated = { summary: { gate: [{ check: "pass rate", ok: true }] } };
	assert.equal(isGatedComparison({ ...gated, taskSet: "default" }), true);
	assert.equal(isGatedComparison({ ...gated, taskSet: "delegation" }), false);
});
