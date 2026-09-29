import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { allRowsPassed, rowRange } from "./acceptance-report.mjs";
import { freePath, isGatedComparison, summarize, unknownArgs, withoutHome } from "./eval-quality.mjs";

test("the gate requires every manifest row, not a fixed count", () => {
	assert.equal(allRowsPassed({ total: 55, passed: 55 }), true);
	// 46 passing rows once counted as complete; with 55 rows that must fail.
	assert.equal(allRowsPassed({ total: 55, passed: 46 }), false);
	assert.equal(allRowsPassed({ total: 0, passed: 0 }), false);
});

test("the report labels its row range from the rows it has", () => {
	assert.equal(rowRange([{ id: "A01" }, { id: "A55" }]), "A01-A55");
	assert.equal(rowRange([]), "no rows");
});

test("an eval result never overwrites an earlier one from the same day", () => {
	const dir = mkdtempSync(join(tmpdir(), "ultron-free-path-"));
	try {
		const first = join(dir, "2026-09-26-hard-model.json");
		assert.equal(freePath(first), first);
		writeFileSync(first, "{}");
		assert.equal(freePath(first), join(dir, "2026-09-26-hard-model-2.json"));
		writeFileSync(join(dir, "2026-09-26-hard-model-2.json"), "{}");
		assert.equal(freePath(first), join(dir, "2026-09-26-hard-model-3.json"));
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("recorded commands carry no home directory", () => {
	assert.equal(
		withoutHome("node --import /home/alice/wt/src/x.ts /home/alice/wt/src/cli.ts", "/home/alice"),
		"node --import ~/wt/src/x.ts ~/wt/src/cli.ts",
	);
	assert.equal(withoutHome("ultron", "/home/alice"), "ultron");
});

test("unknownArgs rejects --help and typos so they never start a paid run", () => {
	assert.deepEqual(unknownArgs(["--help"]), ["--help"]);
	assert.deepEqual(unknownArgs(["--tasks", "delegation", "--trails", "1"]), ["--trails", "1"]);
	assert.deepEqual(unknownArgs(["--tasks", "hard", "--keep-all", "--only", "a,b", "--self-check"]), []);
});

/** A finished eval run as eval-quality.mjs records it (only the fields the summary reads). */
function run(variant, trial, { passed = true, infrastructure = false, durationMs = 60_000, cost = 0.1 } = {}) {
	return { variant, task: "task", category: "bugfix", trial, passed, infrastructure, durationMs, cost };
}

test("a single-variant run passes when its runs pass, with the comparison gates skipped", () => {
	const summary = summarize([run("ultron", 1), run("ultron", 2)], ["ultron"]);
	assert.equal(summary.passed, true);
	assert.equal(summary.comparison, "skipped");
	assert.deepEqual(
		summary.skipped.map((entry) => entry.check),
		["pass rate", "median latency", "cost"],
	);
	assert.match(summary.skipped[0].detail, /needs both pi and ultron; this run has only ultron/);
	// Nothing lands in `gate`, so the release gate never reads a one-variant run as a comparison.
	assert.deepEqual(summary.gate, []);
	assert.equal(isGatedComparison({ taskSet: "default", summary }), false);
});

test("a single-variant run fails when a run fails or too many runs are infrastructure", () => {
	const failed = summarize([run("ultron", 1), run("ultron", 2, { passed: false })], ["ultron"]);
	assert.equal(failed.passed, false);
	assert.deepEqual(failed.checks, [{ check: "runs", ok: false, detail: "ultron: 1 of 2 measured runs passed" }]);

	const flaky = summarize(
		[run("pi", 1), run("pi", 2, { passed: false, infrastructure: true })],
		["pi"],
	);
	assert.equal(flaky.passed, false);
	assert.ok(flaky.checks.some((entry) => entry.check === "coverage" && entry.ok === false));
	// Coverage failures still stay out of the release gate for one-variant runs.
	assert.equal(isGatedComparison({ taskSet: "default", summary: flaky }), false);

	assert.equal(summarize([run("ultron", 1, { infrastructure: true })], ["ultron"]).passed, false);
});

test("two-variant gating is unchanged", () => {
	const both = (candidate) => summarize([run("pi", 1), run("pi", 2), ...candidate], ["pi", "ultron"]);

	const passing = both([run("ultron", 1), run("ultron", 2)]);
	assert.equal(passing.passed, true);
	assert.equal(passing.comparison, undefined);
	assert.equal(passing.skipped, undefined);
	assert.deepEqual(
		passing.gate.map((entry) => [entry.check, entry.ok]),
		[
			["pass rate", true],
			["median latency", true],
			["cost", true],
		],
	);
	assert.equal(isGatedComparison({ taskSet: "default", summary: passing }), true);
	assert.equal(isGatedComparison({ taskSet: "hard", summary: passing }), false);

	// A 50-point pass-rate drop fails the gate, as before.
	const regressed = both([run("ultron", 1), run("ultron", 2, { passed: false })]);
	assert.equal(regressed.passed, false);
	assert.equal(regressed.gate.find((entry) => entry.check === "pass rate").ok, false);

	// Slower than the latency threshold fails too.
	const slow = both([run("ultron", 1, { durationMs: 600_000 }), run("ultron", 2, { durationMs: 600_000 })]);
	assert.equal(slow.gate.find((entry) => entry.check === "median latency").ok, false);
	assert.equal(slow.passed, false);
});

test("Loki is off in comparisons unless --loki, which logs every check", async () => {
	const { lokiEnv, summarizeLoki, summarizeLokiLog } = await import("./eval-quality.mjs");
	assert.deepEqual(unknownArgs(["--tasks", "hard", "--loki"]), []);
	assert.deepEqual(lokiEnv(false, "/x/loki.jsonl"), { ULTRON_LOKI: "off", ULTRON_LOKI_AUTOINIT: "off" });
	assert.deepEqual(lokiEnv(true, "/x/loki.jsonl"), { ULTRON_LOKI: "on", ULTRON_LOKI_LOG: "/x/loki.jsonl" });
	const log = [
		{ phase: "setup", outcome: "on", ms: 210 },
		{ guard: "Loki", phase: "before_write", outcome: "allowed", ms: 120.4 },
		{ guard: "Loki", phase: "before_write", outcome: "blocked", ms: 130, detail: "[Loki] a.py:1: loki/secret" },
		{ guard: "Loki", phase: "before_write", outcome: "unchecked", ms: 5000 },
		{ guard: "Loki", phase: "after_cell", outcome: "findings", ms: 300 },
		{ guard: "extension", phase: "before_write", outcome: "allowed", ms: 1 },
	]
		.map((entry) => JSON.stringify(entry))
		.join("\n");
	const totals = summarizeLokiLog(`${log}\nnot json\n`);
	assert.deepEqual(totals, {
		checks: 3,
		blocked: 1,
		unchecked: 1,
		afterChecks: 1,
		afterFindings: 1,
		ms: 5550,
		setupMs: 210,
		blocks: ["[Loki] a.py:1: loki/secret"],
	});
	const byVariant = summarizeLoki([{ task: "t1", variant: "ultron", loki: totals }, { task: "t2", variant: "pi" }], ["pi", "ultron"]);
	assert.deepEqual(Object.keys(byVariant), ["ultron"]);
	assert.deepEqual(byVariant.ultron.blocks, ["t1: [Loki] a.py:1: loki/secret"]);
});
