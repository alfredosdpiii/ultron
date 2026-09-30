import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import * as deep from "../evals/quality/tasks-delegation-deep.mjs";
import { AGENT_TURN_SECONDS, DEEP, ORCHESTRATION_TURNS, solutions, TURNS_PER_SERVICE } from "../evals/quality/tasks-delegation-solutions.mjs";
import { BUGS, FILES, FIXED, fixedFiles, SERVICES, TIME_BUDGET_MS, tasks } from "../evals/quality/tasks-delegation.mjs";
import { childUptake, formatRunLine, isGatedComparison, summarizeUptake, verifyMetrics } from "./eval-quality.mjs";

const task = tasks().find((entry) => entry.id === "six-services");
const deepTask = tasks().find((entry) => entry.id === "six-services-deep");

/** Words that would name a runtime feature or strategy; the prompts must stay agent-neutral. */
const STRATEGY_WORDS =
	/\b(ultron|rlm|pi|spawn\w*|sub-?agents?|agents?|delegat\w*|parallel\w*|concurren\w*|simultaneous\w*|child\w*|workers?|background|async\w*|threads?|fork\w*|jobs|split|at the same time)\b/i;

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
	assert.deepEqual(
		tasks().map((entry) => entry.id),
		["six-services", "six-services-deep", "twelve-tickets"],
	);
	assert.equal(task.timeBudgetMs, TIME_BUDGET_MS);
	for (const prompt of task.prompts) {
		assert.doesNotMatch(prompt, STRATEGY_WORDS);
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

// ---------------------------------------------------------------------------------------------------------------
// six-services-deep
// ---------------------------------------------------------------------------------------------------------------

function deepProject(extra = {}) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-delegation-deep-test-"));
	const { files, hidden } = deepTask.build();
	for (const [path, content] of Object.entries({ ...files, ...extra, ...hidden })) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		writeFileSync(join(dir, path), content);
	}
	return dir;
}

/**
 * The service's original harness without waiting: `virtual` advances a fake clock by every sleep, so the logged run
 * looks as long as a real one (a forged run: what the evidence check cannot tell apart, used here to test the rest
 * of the check quickly); otherwise the sleeps are simply skipped and the logged run is short.
 */
function fastHarness(dir, service, { virtual = false, env = {} } = {}) {
	const code = [
		"import time, runpy, sys",
		virtual
			? "shift = [0.0]; real = time.time; time.sleep = lambda s: shift.__setitem__(0, shift[0] + s); time.time = lambda: real() + shift[0]"
			: "time.sleep = lambda s: None",
		"sys.argv = ['harness.py']",
		"runpy.run_path('harness.py', run_name='__main__')",
	].join("\n");
	const run = spawnSync("python3", ["-B", "-c", code], { cwd: join(dir, "services", service), encoding: "utf8", env: { ...process.env, ...env } });
	const failed = run.stdout.match(/^stage (\d+)\/5 \S+: FAILED\n(.*)$/m);
	return { status: run.status, firstFailure: failed ? Number(failed[1]) : null, failure: failed ? failed[2] : null, stdout: run.stdout };
}

function deepCheck(dir) {
	const run = spawnSync("python3", ["check_delegation_deep_hidden.py"], { cwd: dir, encoding: "utf8" });
	return { status: run.status, metrics: verifyMetrics(run.stdout), stdout: run.stdout };
}

const everyDeep = (indexes) => Object.fromEntries(deep.SERVICES.map((service) => [service, indexes]));

test("six-services-deep: six services with a slow staged harness each, an agent-neutral prompt and a 5 minute budget", () => {
	assert.equal(deepTask.timeBudgetMs, 5 * 60 * 1000);
	for (const prompt of deepTask.prompts) {
		assert.doesNotMatch(prompt, STRATEGY_WORDS);
		assert.match(prompt, /within 5 minutes/);
		assert.match(prompt, /independent/);
		assert.match(prompt, /harness passed on its final code/);
	}
	assert.equal(deep.SERVICES.length, 6);
	const head = deep.FILES[`services/${deep.SERVICES[0]}/harness.py`].split("# Service checks")[0];
	const tail = deep.FILES[`services/${deep.SERVICES[0]}/harness.py`].split("# Runner")[1];
	for (const service of deep.SERVICES) {
		const own = Object.keys(deep.FILES).filter((path) => path.startsWith(`services/${service}/`));
		assert.ok(own.includes(`services/${service}/SPEC.md`), service);
		// The harness is the only check shipped: no quick tests.
		assert.ok(!own.some((path) => path.includes("/tests/")), service);
		const harness = deep.FILES[`services/${service}/harness.py`];
		// Every harness shares the same runner and timing, and has five stages.
		assert.ok(harness.startsWith(head) && harness.endsWith(tail), `${service}: harness framework differs`);
		assert.match(harness, new RegExp(`^BUILD_SECONDS = ${deep.HARNESS.buildSeconds}$`, "m"));
		assert.match(harness, new RegExp(`^STAGE_SECONDS = ${deep.HARNESS.stageSeconds}$`, "m"));
		assert.match(harness, new RegExp(`^SERVICE = "${service}"$`, "m"));
		assert.match(harness, new RegExp(`^PACKAGE = "${deep.PACKAGES[service]}"$`, "m"));
		const sources = own.filter((path) => path.startsWith(`services/${service}/${deep.PACKAGES[service]}/`) && path.endsWith(".py"));
		const lines = sources.reduce((total, path) => total + deep.FILES[path].split("\n").length, 0);
		assert.ok(sources.length >= 5 && lines >= 350, `${service}: ${sources.length} files, ${lines} lines`);
		assert.equal(deep.BUGS[service].length, 3, service);
		assert.ok(new Set(deep.BUGS[service].map((bug) => bug.path)).size >= 2, `${service}: bugs spread across files`);
		assert.ok(deep.BUGS[service].every((bug) => !bug.path.endsWith("__init__.py")), service);
	}
});

test("six-services-deep: the budget separates the parallel loops from the sequential floor", () => {
	const { buildSeconds, stageSeconds } = deep.HARNESS;
	const run = (stage) => buildSeconds + stage * stageSeconds;
	// Per service: a run failing at each bug's stage, then a passing run.
	const harnessSeconds = deep.BUG_STAGES.reduce((total, stage) => total + run(stage), 0) + run(deep.HARNESS.stages);
	assert.equal(harnessSeconds, 66);
	const turns = deep.BUG_STAGES.length * DEEP.turnsPerLayer * DEEP.turnSeconds;
	const around = DEEP.orchestrationTurns * DEEP.turnSeconds;
	assert.ok((harnessSeconds + turns + around) * 1000 < deep.TIME_BUDGET_MS * 0.6, "parallel reference");
	// The slow end (15 s x 3 turns per layer) still fits in parallel.
	assert.ok((harnessSeconds + 3 * 3 * 15 + 2 * 15) * 1000 < deep.TIME_BUDGET_MS);
	// One service at a time is over budget on harness time alone, with no model time at all.
	assert.ok(6 * harnessSeconds * 1000 > deep.TIME_BUDGET_MS * 1.25);
	const solution = solutions[deepTask.id];
	assert.equal(solution.expectWithinBudget, true);
	assert.deepEqual(
		solution.alternatives.filter((entry) => entry.expectWithinBudget !== undefined).map((entry) => [entry.expect, entry.expectWithinBudget]),
		[["pass", false]],
	);
	const wrong = solution.alternatives.filter((entry) => entry.expect === "fail").map((entry) => entry.name);
	assert.equal(wrong.filter((name) => name.includes("untouched")).length, 6);
	assert.equal(wrong.filter((name) => name.includes("all fixed but")).length, 18);
	for (const kind of ["no harness run", "one more edit", "edited to drop its sleeps", "a copy of the harness", "time.sleep patched out"])
		assert.ok(wrong.some((name) => name.includes(kind)), kind);
});

test("six-services-deep: the harness reveals each service's bugs one layer at a time", { timeout: 300_000 }, () => {
	// Prefixes of the fixes: nothing, bug 0, bugs 0 and 1, all three. Then each bug left alone.
	const cases = [
		...[[], [0], [0, 1]].map((fixes, layer) => ({ fixes, want: deep.BUG_STAGES[layer] })),
		{ fixes: [0, 1, 2], want: null },
		...[0, 1, 2].map((index) => ({ fixes: [0, 1, 2].filter((other) => other !== index), want: deep.BUG_STAGES[index] })),
	];
	for (const { fixes, want } of cases) {
		const dir = deepProject(deep.fixedFiles(everyDeep(fixes)));
		try {
			for (const service of deep.SERVICES) {
				const outcome = fastHarness(dir, service);
				assert.equal(outcome.firstFailure, want, `${service} with fixes [${fixes}]:\n${outcome.stdout}`);
				assert.equal(outcome.status === 0, want === null, `${service} with fixes [${fixes}]`);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	}
});

test("six-services-deep: every bug left alone fails the harness at its own stage on every run", { timeout: 600_000 }, () => {
	// No bug may depend on object ids, hash randomization or allocation: the same stage and the same counterexample
	// on three runs with different hash seeds, the last after a harmless edit to the package (a comment, an unused
	// function and a few thousand allocations at import), which moves every later object in memory.
	const harmless =
		"\n# harmless edit\n\n\ndef _unused_helper():\n    return None\n\n\n_padding = [object() for _ in range(1237)]\ndel _padding[::3]\n";
	const runs = [{ PYTHONHASHSEED: "0" }, { PYTHONHASHSEED: "1" }, { PYTHONHASHSEED: "random", edit: true }];
	for (const index of [0, 1, 2]) {
		const dir = deepProject(deep.fixedFiles(everyDeep([0, 1, 2].filter((other) => other !== index))));
		try {
			for (const service of deep.SERVICES) {
				const seen = [];
				for (const { edit, ...env } of runs) {
					if (edit) writeFileSync(join(dir, "services", service, deep.PACKAGES[service], "__init__.py"), harmless, { flag: "a" });
					const outcome = fastHarness(dir, service, { env });
					assert.equal(outcome.firstFailure, deep.BUG_STAGES[index], `${service} bug ${index} (${JSON.stringify(env)}):\n${outcome.stdout}`);
					seen.push(outcome.failure);
				}
				assert.equal(new Set(seen).size, 1, `${service} bug ${index}: the counterexample changed between runs:\n${seen.join("\n")}`);
			}
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	}
});

test("six-services-deep: the hidden check needs the hidden tests and a real passing harness run on the final source", { timeout: 300_000 }, () => {
	const services = (value) => Object.fromEntries(deep.SERVICES.map((service) => [service, value]));
	// Untouched: nothing passes.
	let dir = deepProject();
	try {
		const outcome = deepCheck(dir);
		assert.notEqual(outcome.status, 0);
		assert.deepEqual(outcome.metrics.services, services(false));
		assert.deepEqual(outcome.metrics.tests, services(false));
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
	// Every fix: the hidden tests pass, but without a harness run no service counts.
	dir = deepProject(deep.FIXED);
	try {
		let outcome = deepCheck(dir);
		assert.deepEqual(outcome.metrics.tests, services(true), outcome.stdout);
		assert.deepEqual(outcome.metrics.evidence, services(false));
		assert.equal(outcome.metrics.passed, 0);
		// A run with the sleeps skipped is too short to count.
		for (const service of deep.SERVICES) assert.equal(fastHarness(dir, service).status, 0);
		assert.equal(deepCheck(dir).metrics.passed, 0);
		// A full-length run (forged here with a virtual clock) counts ...
		for (const service of deep.SERVICES) assert.equal(fastHarness(dir, service, { virtual: true }).status, 0);
		outcome = deepCheck(dir);
		assert.equal(outcome.status, 0, outcome.stdout);
		assert.equal(outcome.metrics.passed, 6);
		// ... until the package changes after it, or the harness is edited.
		const [first, second] = deep.SERVICES;
		writeFileSync(join(dir, "services", first, deep.PACKAGES[first], "__init__.py"), "\n", { flag: "a" });
		writeFileSync(join(dir, "services", second, "harness.py"), "\n", { flag: "a" });
		outcome = deepCheck(dir);
		assert.equal(outcome.metrics.passed, 4);
		assert.equal(outcome.metrics.services[first], false);
		assert.equal(outcome.metrics.services[second], false);
		assert.match(outcome.stdout, new RegExp(`${first}: FAIL \\(no passing harness run on the final source`));
		assert.match(outcome.stdout, new RegExp(`${second}: FAIL \\(harness.py was modified`));
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
	// Each bug left alone fails exactly its own service's hidden tests.
	for (const service of deep.SERVICES)
		for (const index of [0, 1, 2]) {
			dir = deepProject(deep.fixedFiles({ ...everyDeep([0, 1, 2]), [service]: [0, 1, 2].filter((other) => other !== index) }));
			try {
				const run = spawnSync("python3", ["-B", "test_hidden.py"], { cwd: join(dir, "services", service), encoding: "utf8" });
				assert.notEqual(run.status, 0, `${service} bug ${index}`);
			} finally {
				rmSync(dir, { recursive: true, force: true });
			}
		}
});
