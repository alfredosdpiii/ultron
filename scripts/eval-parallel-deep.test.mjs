import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import * as pipeline from "../evals/quality/tasks-parallel-deep.mjs";
import { PIPELINE, solutions } from "../evals/quality/tasks-parallel-solutions.mjs";
import { tasks } from "../evals/quality/tasks-parallel.mjs";
import { verifyMetrics } from "./eval-quality.mjs";

const task = tasks().find((entry) => entry.id === "slow-pipeline-deep");

/** Words that would name a runtime feature or a strategy; the prompt must stay agent-neutral. */
const STRATEGY_WORDS =
	/\b(ultron|rlm|pi|yield_after|handles?|spawn\w*|sub-?agents?|agents?|delegat\w*|parallel\w*|concurren\w*|simultaneous\w*|child\w*|workers?|background\w*|async\w*|threads?|fork\w*|jobs?|events?|detach\w*|nohup|poll\w*|wait\w*|meanwhile|split|at the same time|at once)\b|&/i;

function writeTree(dir, files) {
	for (const [path, content] of Object.entries(files)) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		writeFileSync(join(dir, path), content, content.startsWith("#!") ? { mode: 0o755 } : undefined);
	}
}

function project(extra = {}) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-pipeline-test-"));
	const { files, hidden } = task.build();
	writeTree(dir, { ...files, ...hidden, ...extra });
	return dir;
}

const sh = (dir, command) => spawnSync("sh", ["-c", command], { cwd: dir, encoding: "utf8" });

function check(dir) {
	const run = spawnSync("python3", ["check_pipeline_hidden.py"], { cwd: dir, encoding: "utf8" });
	return { status: run.status, metrics: verifyMetrics(run.stdout), stdout: run.stdout };
}

const readLog = (dir, name) =>
	readFileSync(join(dir, ".ops", `${name}.jsonl`), "utf8")
		.trim()
		.split("\n")
		.map((line) => JSON.parse(line));
const writeLog = (dir, name, entries) =>
	writeFileSync(join(dir, ".ops", `${name}.jsonl`), entries.map((entry) => `${JSON.stringify(entry)}\n`).join(""));

/**
 * Runs every ops step in a copy of `dir` whose ops scripts wait one second instead of minutes, then brings the logs
 * (and the snapshot) back into `dir`, backdated to full-length runs when `backdate` is set. The runs are otherwise
 * real: the same client/ and ledger/ code, the same checks, the same digests.
 */
function fastRuns(dir, { backdate = true } = {}) {
	const fast = mkdtempSync(join(tmpdir(), "ultron-pipeline-fast-"));
	try {
		cpSync(dir, fast, { recursive: true });
		for (const name of ["provision", "snapshot", "loadtest", "replay"]) {
			const path = join(fast, "ops/lib", `${name}.py`);
			writeFileSync(path, readFileSync(path, "utf8").replace(/^SECONDS = \d+$/m, "SECONDS = 1"));
		}
		assert.equal(sh(fast, "./ops/provision.sh").status, 0);
		const { tenant, max_batch: batch } = readLog(fast, "provision").at(-1);
		const config = join(fast, "client/config.py");
		writeFileSync(
			config,
			readFileSync(config, "utf8")
				.replace(/^TENANT = .*$/m, `TENANT = "${tenant}"`)
				.replace(/^MAX_BATCH = .*$/m, `MAX_BATCH = ${batch}`),
		);
		cpSync(config, join(dir, "client/config.py"));
		const loadtest = sh(fast, "./ops/loadtest.sh");
		assert.equal(sh(fast, "./ops/snapshot.sh").status, 0);
		const { snapshot } = readLog(fast, "snapshot").at(-1);
		const replay = sh(fast, `./ops/replay.sh ${snapshot}`);
		if (backdate)
			for (const name of ["provision", "snapshot", "loadtest", "replay"])
				writeLog(
					fast,
					name,
					readLog(fast, name).map((entry) => ({ ...entry, started: entry.finished - pipeline.STEPS[name].seconds })),
				);
		rmSync(join(dir, ".ops"), { recursive: true, force: true });
		cpSync(join(fast, ".ops"), join(dir, ".ops"), { recursive: true });
		const summary = (output, prefix) => output.split("\n").find((line) => line.startsWith(prefix));
		return { loadtest, replay, lines: [summary(loadtest.stdout, "loadtest:"), summary(replay.stdout, "replay:")] };
	} finally {
		rmSync(fast, { recursive: true, force: true });
	}
}

test("slow-pipeline-deep: an agent-neutral prompt that states every step's duration and a 4.5 minute budget", () => {
	assert.equal(task.category, "parallel");
	assert.equal(task.timeBudgetMs, 270_000);
	for (const prompt of task.prompts) {
		assert.doesNotMatch(prompt, STRATEGY_WORDS);
		assert.match(prompt, /within 4\.5 minutes/);
		assert.match(prompt, /more than 7 minutes/);
		for (const [pattern, step] of [
			[/provision\.sh` \(about 35 seconds\)/, "provision"],
			[/snapshot\.sh` \(about 2 minutes/, "snapshot"],
			[/loadtest\.sh` \(about 2\.5 minutes\)/, "loadtest"],
			[/replay\.sh <snapshot-id>` \(about 45 seconds\)/, "replay"],
		]) {
			assert.match(prompt, pattern, step);
			assert.match(pipeline.FILES[`ops/lib/${step}.py`], new RegExp(`^SECONDS = ${pipeline.STEPS[step].seconds}$`, "m"), step);
			assert.match(pipeline.HIDDEN["check_pipeline_hidden.py"], new RegExp(`^MIN_${step.toUpperCase()} = ${pipeline.STEPS[step].minSeconds}$`, "m"), step);
		}
	}
	// ops/ is pinned file by file.
	assert.deepEqual(
		Object.keys(JSON.parse(pipeline.HIDDEN["ops_originals.json"])),
		Object.keys(pipeline.FILES)
			.filter((path) => path.startsWith("ops/"))
			.sort(),
	);
});

test("slow-pipeline-deep: the budget sits between the overlapped plan and waiting for all first steps", () => {
	const { provision, snapshot, loadtest, replay } = Object.fromEntries(Object.entries(pipeline.STEPS).map(([name, step]) => [name, step.seconds]));
	const turn = PIPELINE.turnSeconds;
	const fixes = PIPELINE.fixTurns * turn;
	const budget = task.timeBudgetMs / 1000;
	// Overlapped: provision and snapshot at once; the load test as soon as provision (and the uploader fix) is done;
	// the replay as soon as the snapshot is; one turn each to start the load test, the replay and write the report.
	const loadtestStart = turn + Math.max(provision, fixes) + turn;
	const replayStart = Math.max(turn + snapshot, loadtestStart + fixes) + turn;
	const overlapped = Math.max(loadtestStart + loadtest, replayStart + replay) + turn;
	const waitAll = turn + Math.max(snapshot, provision, 2 * fixes) + turn + Math.max(loadtest, replay) + turn;
	const sequential = turn + provision + fixes + turn + loadtest + fixes + snapshot + turn + replay + turn;
	assert.ok(overlapped <= budget - 45, `overlapped ${overlapped}s leaves at least 45 s of slack`);
	assert.ok(waitAll >= budget + 20, `waiting for both first steps (${waitAll}s) is over budget`);
	assert.ok(sequential >= budget + 150, `sequential ${sequential}s`);
	// The prompt's "more than 7 minutes" one step after another holds with the model's turns counted.
	assert.ok(sequential > 7 * 60);
	const solution = solutions[task.id];
	assert.equal(solution.expectWithinBudget, true);
	assert.deepEqual(
		solution.alternatives.map((entry) => [entry.expect, entry.expectWithinBudget]),
		[["pass", false], ["pass", false], ...Array(7).fill(["fail", undefined])],
	);
});

test("slow-pipeline-deep: each bug fails its unit test and its ops step; the fixes pass the hidden cases", { timeout: 60_000 }, () => {
	const dir = project();
	try {
		assert.notEqual(sh(dir, "python3 -m unittest discover -s tests/unit").status, 0);
		const buggy = fastRuns(dir, { backdate: false });
		assert.equal(buggy.loadtest.status, 1);
		assert.match(buggy.loadtest.stdout, /FAILED: the service received \d+ of \d+ records/);
		assert.equal(buggy.replay.status, 1);
		assert.match(buggy.lines[1], /, [1-9]\d* mismatched in/);
		writeTree(dir, pipeline.FIXED);
		assert.equal(sh(dir, "python3 -m unittest discover -s tests/unit").status, 0);
		assert.equal(sh(dir, "python3 -m unittest test_uploader_hidden test_ledger_hidden").status, 0);
		for (const name of Object.keys(pipeline.BUGS)) {
			const other = project(pipeline.fixedFiles([name]));
			try {
				assert.notEqual(sh(other, "python3 -m unittest test_uploader_hidden test_ledger_hidden").status, 0, `only ${name} fixed`);
			} finally {
				rmSync(other, { recursive: true, force: true });
			}
		}
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("slow-pipeline-deep: the hidden check needs full-length passing runs on the final code, quoted in REPORT.md", { timeout: 60_000 }, () => {
	const dir = project(pipeline.FIXED);
	try {
		// Guessed ids: the load test refuses an unprovisioned tenant and the replay an unknown snapshot, both at once.
		const refused = sh(dir, "./ops/loadtest.sh; ./ops/replay.sh s-9c1d7e");
		assert.match(refused.stdout, /tenant 'unprovisioned' \(client\/config\.py\) was never provisioned/);
		assert.match(refused.stdout, /no snapshot 's-9c1d7e' was taken/);
		// Short runs (the real scripts' waits cut) do not count.
		const short = fastRuns(dir, { backdate: false });
		writeFileSync(join(dir, "REPORT.md"), `${short.lines.join("\n")}\n`);
		let result = check(dir);
		assert.equal(result.status, 1);
		assert.match(result.stdout, /no complete load test run against a provisioned tenant/);
		assert.match(result.stdout, /no complete replay run of a snapshot/);
		// Full-length runs pass.
		const full = fastRuns(dir);
		assert.match(full.lines[0], /^loadtest: tenant t-[0-9a-f]{6}, 50 rounds passed, 0 failed in [\d.]+s \[run [0-9a-f]{8}\]$/);
		assert.match(full.lines[1], /^replay: snapshot s-[0-9a-f]{6}, 600 events, \d+ accounts, 0 mismatched in [\d.]+s \[run [0-9a-f]{8}\]$/);
		writeFileSync(join(dir, "REPORT.md"), `# Release\n\n${full.lines.join("\n")}\n`);
		result = check(dir);
		assert.equal(result.status, 0, result.stdout);
		assert.deepEqual(result.metrics, { ops: true, tests: true, loadtest: true, replay: true, report: true });
		// Each missing piece fails on its own.
		const failsWith = (mutate, pattern, restore) => {
			mutate();
			const outcome = check(dir);
			assert.equal(outcome.status, 1, String(pattern));
			assert.match(outcome.stdout, pattern);
			restore();
			assert.equal(check(dir).status, 0);
		};
		const edit = (path, change) => {
			const before = readFileSync(join(dir, path), "utf8");
			return [() => writeFileSync(join(dir, path), change(before)), () => writeFileSync(join(dir, path), before)];
		};
		const report = edit("REPORT.md", (text) => text.replace(/^replay:.*$/m, ""));
		failsWith(report[0], /REPORT.md does not contain the summary line of a passing replay/, report[1]);
		const clientEdit = edit("client/uploader.py", (text) => `${text}\n# Reviewed.\n`);
		failsWith(clientEdit[0], /no passing load test run on the final client\/ code/, clientEdit[1]);
		const ledgerEdit = edit("ledger/ledger.py", (text) => `${text}\n# Reviewed.\n`);
		failsWith(ledgerEdit[0], /no passing replay run on the final ledger\/ code/, ledgerEdit[1]);
		const provisionLog = edit(".ops/provision.jsonl", () => "");
		failsWith(provisionLog[0], /no complete load test run against a provisioned tenant/, provisionLog[1]);
		const snapshotLog = edit(".ops/snapshot.jsonl", () => "");
		failsWith(snapshotLog[0], /no complete replay run of a snapshot/, snapshotLog[1]);
		const runner = edit("ops/lib/loadtest.py", (text) => text.replace("SECONDS = 150", "SECONDS = 1"));
		failsWith(runner[0], /ops\/lib\/loadtest\.py was modified/, runner[1]);
		failsWith(
			() => writeFileSync(join(dir, "ops/lib/random.py"), "from random import *\n"),
			/ops\/lib\/random\.py was added under ops\//,
			() => rmSync(join(dir, "ops/lib/random.py")),
		);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("slow-pipeline-deep: project code cannot speed up the clock or the waits of an ops run", { timeout: 60_000 }, () => {
	// client/ and ledger/ are imported by the load test and the replay; patching time there must not shorten a run.
	const patch = "import time as _time\n_real = _time.time\n_time.time = lambda: _real() * 2\n_time.sleep = lambda seconds: None\n";
	const dir = project({ ...pipeline.FIXED, "client/__init__.py": patch, "ledger/__init__.py": patch });
	try {
		const started = Date.now();
		fastRuns(dir, { backdate: false });
		// Four steps of one second each, and the two patched ones still waited.
		assert.ok(Date.now() - started >= 3500);
		for (const name of ["loadtest", "replay"]) {
			const run = readLog(dir, name).at(-1);
			assert.equal(run.ok, true, name);
			assert.ok(run.finished - run.started >= 0.9 && run.finished - run.started < 10, `${name}: ${run.finished - run.started}`);
		}
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});
