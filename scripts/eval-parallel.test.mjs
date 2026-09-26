import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { AGENT_TURN_SECONDS, FIX_TURNS, solutions } from "../evals/quality/tasks-parallel-solutions.mjs";
import { FIXED, SUITE, TIME_BUDGET_MS, tasks } from "../evals/quality/tasks-parallel.mjs";
import { formatRunTime, isGatedComparison, summarize, summarizeTiming } from "./eval-quality.mjs";

const [task] = tasks();

function project(extra = {}) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-parallel-test-"));
	const { files, hidden } = task.build();
	for (const [path, content] of Object.entries({ ...files, ...hidden, ...extra })) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		writeFileSync(join(dir, path), content, content.startsWith("#!") ? { mode: 0o755 } : undefined);
	}
	return dir;
}

const check = (dir) => spawnSync("python3", ["check_parallel_hidden.py"], { cwd: dir, encoding: "utf8" });

/** A run of the real runner with its sleeps removed, then its log entry backdated to look like a full-length run. */
function fastSuiteRun(dir, seconds) {
	const runner = readFileSync(join(dir, "tests/integration/runner.py"), "utf8").replace("time.sleep(SUITE_SECONDS / CHECKS)", "pass");
	writeFileSync(join(dir, "tests/integration/_fast.py"), runner);
	const run = spawnSync("python3", ["tests/integration/_fast.py"], { cwd: dir, encoding: "utf8" });
	assert.equal(run.status, 0, run.stdout + run.stderr);
	rmSync(join(dir, "tests/integration/_fast.py"));
	const log = join(dir, ".integration/runs.jsonl");
	const entries = readFileSync(log, "utf8").trim().split("\n").map((line) => JSON.parse(line));
	const entry = entries.at(-1);
	entry.started = entry.finished - seconds;
	writeFileSync(log, entries.map((each) => `${JSON.stringify(each)}\n`).join(""));
	return entry.line;
}

test("the parallel set is one timed task with an agent-neutral prompt", () => {
	assert.equal(tasks().length, 1);
	assert.equal(task.id, "slow-suite-and-fixes");
	assert.equal(task.timeBudgetMs, TIME_BUDGET_MS);
	for (const prompt of task.prompts) assert.doesNotMatch(prompt, /\b(ultron|rlm|yield_after|handles?|background|async\w*|in parallel)\b/i);
	// The budget sits between a suite overlapped with the fix work and the suite followed by it.
	const turn = AGENT_TURN_SECONDS * 1000;
	assert.ok(SUITE.seconds * 1000 + turn < TIME_BUDGET_MS);
	assert.ok(SUITE.seconds * 1000 + FIX_TURNS * turn + turn > TIME_BUDGET_MS);
	const solution = solutions[task.id];
	assert.equal(solution.expectWithinBudget, true);
	assert.deepEqual(
		solution.alternatives.map((entry) => [entry.expect, entry.expectWithinBudget]),
		[["pass", false], ["fail", undefined], ["fail", undefined], ["fail", undefined], ["fail", undefined]],
	);
});

test("the hidden check needs the fixes and the line of a real, full-length suite run", { timeout: 60_000 }, () => {
	const dir = project();
	try {
		assert.notEqual(spawnSync("python3", ["-m", "unittest", "discover", "-s", "tests/unit"], { cwd: dir }).status, 0);
		for (const [path, content] of Object.entries(FIXED)) writeFileSync(join(dir, path), content);
		writeFileSync(join(dir, "REPORT.md"), "integration: 42 passed, 0 failed in 150.0s [run 0badc0de]\n");
		assert.match(check(dir).stdout, /no complete integration run/);
		const short = fastSuiteRun(dir, 5);
		writeFileSync(join(dir, "REPORT.md"), `# Report\n\n${short}\n`);
		assert.match(check(dir).stdout, /no complete integration run/);
		const line = fastSuiteRun(dir, 150);
		assert.match(line, /^integration: 42 passed, 0 failed in [\d.]+s \[run [0-9a-f]{8}\]$/);
		writeFileSync(join(dir, "REPORT.md"), `# Report\n\n${line}\n`);
		const passing = check(dir);
		assert.equal(passing.status, 0, passing.stdout);
		// Editing the sleeps out of the suite is caught.
		const runner = join(dir, "tests/integration/runner.py");
		writeFileSync(runner, readFileSync(runner, "utf8").replace("SUITE_SECONDS = 150", "SUITE_SECONDS = 1"));
		assert.match(check(dir).stdout, /runner.py was modified/);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("wall time is reported per run and per variant but never gates", () => {
	const run = (variant, trial, durationMs, passed) => ({
		task: task.id,
		category: "parallel",
		variant,
		trial,
		passed,
		durationMs,
		timeBudgetMs: TIME_BUDGET_MS,
		withinBudget: durationMs <= TIME_BUDGET_MS,
		toolsByName: {},
	});
	const records = [run("pi", 1, 300_000, true), run("pi", 2, 280_000, true), run("ultron", 1, 190_000, true), run("ultron", 2, 200_000, false)];
	const timing = summarizeTiming(records.filter((record) => record.variant === "ultron"));
	assert.equal(timing.runs, 2);
	assert.equal(timing.withinBudget, 2);
	assert.equal(timing.passedWithinBudget, 1);
	assert.equal(timing.medianDurationMs, 195_000);
	assert.equal(summarizeTiming([{ durationMs: 1 }]), null);
	const summary = summarize(records, ["pi", "ultron"]);
	assert.equal(summary.byVariant.pi.timing.withinBudget, 0);
	assert.equal(summary.byVariant.ultron.timing.withinBudget, 2);
	assert.deepEqual(
		summary.gate.map((entry) => entry.check),
		["pass rate", "median latency", "cost"],
	);
	assert.match(formatRunTime(records[0]), /^TIME\s+300\.0s \/ budget 240\.0s OVER {2}PASS pi slow-suite-and-fixes#1$/);
	assert.match(formatRunTime({ ...records[2], timeBudgetMs: undefined }), /^TIME\s+190\.0s {2}PASS ultron/);
});

test("the release gate reads only full comparisons of the default set", () => {
	const gated = { summary: { gate: [{ check: "pass rate", ok: true }] } };
	assert.equal(isGatedComparison(gated), true);
	assert.equal(isGatedComparison({ ...gated, taskSet: "default" }), true);
	for (const taskSet of ["hard", "judged", "parallel"]) assert.equal(isGatedComparison({ ...gated, taskSet }), false, taskSet);
	assert.equal(isGatedComparison({ taskSet: "default", summary: { gate: [] } }), false);
});
