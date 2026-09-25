import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { tasks as judgedTasks } from "../evals/quality/tasks-judged.mjs";
import {
	buildJudgePrompt,
	collectJudgeFiles,
	fakeJudge,
	JUDGE_MAX_ATTEMPTS,
	JUDGE_MAX_FILE_BYTES,
	judgeRun,
	parseJudgeReply,
	summarizeJudged,
	validateJudgeSpec,
} from "./eval-judge.mjs";
import { summarize, summarizeUptake } from "./eval-quality.mjs";

const rubric = [
	{ id: "correct", points: 3, description: "The answer is correct and complete." },
	{ id: "clear", points: 1, description: "The answer is clear to a newcomer." },
];
const task = { id: "t", prompts: ["Explain x.py"], judge: { rubric, inputs: ["NOTE.md", "x.py"], passAt: 0.75 } };

test("every frozen judged task has a well-formed rubric", () => {
	const all = judgedTasks();
	assert.ok(all.length >= 3 && all.length <= 5);
	for (const item of all) {
		assert.deepEqual(validateJudgeSpec(item.judge), [], item.id);
		assert.equal(typeof item.verify, "string", `${item.id} keeps a deterministic sanity check`);
	}
});

test("rubric validation names each problem", () => {
	const problems = validateJudgeSpec({
		rubric: [
			{ id: "Bad Id", points: 0, description: "short" },
			{ id: "x", points: 1, description: "a real description" },
			{ id: "x", points: 1, description: "a real description" },
		],
		inputs: [],
		passAt: 2,
	});
	assert.equal(problems.length, 6, problems.join("\n"));
});

test("replies are parsed strictly against the rubric", () => {
	const parsed = parseJudgeReply(
		'Sure:\n```json\n{"scores": {"correct": 2, "clear": 1}, "reasons": {"correct": "misses one case"}}\n```',
		rubric,
	);
	assert.deepEqual(parsed.scores, { correct: 2, clear: 1 });
	assert.equal(parsed.reasons.correct, "misses one case");
	assert.equal(parsed.reasons.clear, "");
	assert.equal(parsed.total, 3);
	assert.equal(parsed.max, 4);
	assert.equal(parsed.normalized, 0.75);
	assert.throws(() => parseJudgeReply("great work", rubric), /no JSON object/);
	assert.throws(() => parseJudgeReply('{"scores": {"correct": 4, "clear": 1}}', rubric), /correct must be an integer 0-3/);
	assert.throws(() => parseJudgeReply('{"scores": {"correct": 1.5, "clear": 1}}', rubric), /integer/);
	assert.throws(() => parseJudgeReply('{"scores": {"correct": 1}}', rubric), /clear/);
	assert.throws(() => parseJudgeReply('{"scores": {"correct": 1, "clear": 1, "vibes": 5}}', rubric), /unknown criteria: vibes/);
	assert.throws(() => parseJudgeReply('{"verdict": "good"}', rubric), /"scores"/);
});

test("the judge reads bounded before/after files and the frozen rubric", (t) => {
	const dir = mkdtempSync(join(tmpdir(), "eval-judge-"));
	t.after(() => rmSync(dir, { recursive: true, force: true }));
	writeFileSync(join(dir, "NOTE.md"), "x".repeat(JUDGE_MAX_FILE_BYTES * 2));
	writeFileSync(join(dir, "x.py"), "print(2)\n");
	const files = collectJudgeFiles(dir, task.judge, { "x.py": "print(1)\n" });
	assert.equal(files[0].before, null);
	assert.match(files[0].after, /bytes elided/);
	assert.ok(Buffer.byteLength(files[0].after) < JUDGE_MAX_FILE_BYTES + 100);
	assert.equal(files[1].before, "print(1)\n");
	const prompt = buildJudgePrompt(task, files);
	for (const expected of ["Explain x.py", "- correct (0-3): The answer is correct", "=== x.py (before the agent) ===", "print(2)", '"clear": <0-1>'])
		assert.ok(prompt.includes(expected), expected);
	// A file the agent never wrote is shown as missing, not skipped.
	assert.match(buildJudgePrompt(task, collectJudgeFiles(dir, { ...task.judge, inputs: ["MISSING.md"] })), /does not exist/);
});

test("a malformed reply is re-asked once, then recorded as an error without a score", async () => {
	const prompts = [];
	const replies = ["no idea", '{"scores": {"correct": 3, "clear": 0}, "reasons": {}}'];
	const judged = await judgeRun({
		task,
		files: [],
		model: "p/m",
		call: async (_system, prompt) => {
			prompts.push(prompt);
			return { text: replies.shift(), usage: { totalTokens: 10 } };
		},
	});
	assert.equal(prompts.length, 2);
	assert.match(prompts[1], /could not be used \(no JSON object/);
	assert.equal(judged.attempts[0].error, "no JSON object in the reply");
	assert.equal(judged.normalized, 0.75);
	assert.equal(judged.passed, true);
	assert.equal(judged.model, "p/m");
	assert.equal(typeof judged.rubricDigest, "string");

	const failed = await judgeRun({ task, files: [], model: "p/m", call: fakeJudge(rubric, { mode: "malformed" }) });
	assert.equal(failed.attempts.length, JUDGE_MAX_ATTEMPTS);
	assert.equal(failed.normalized, undefined);
	assert.equal(failed.passed, undefined);
	assert.match(failed.error, /no JSON object/);

	const crashed = await judgeRun({
		task,
		files: [],
		model: "p/m",
		call: async () => {
			throw new Error("429 cooldown");
		},
	});
	assert.equal(crashed.attempts.length, 1);
	assert.match(crashed.error, /judge call failed: 429 cooldown/);
});

const run = (overrides) => ({
	task: "t",
	category: "c",
	variant: "ultron",
	passed: true,
	durationMs: 1000,
	cost: 1,
	toolsByName: {},
	framesSpawned: null,
	rootUnseenBytes: null,
	...overrides,
});

test("uptake aggregates tools, rlm share, frames and unseen bytes, tolerating absent metrics", () => {
	const uptake = summarizeUptake([
		run({ toolsByName: { rlm: 3, bash: 1 }, framesSpawned: 2, rootUnseenBytes: 1000 }),
		run({ toolsByName: { rlm: 1 }, framesSpawned: 0 }),
		run({ toolsByName: { bash: 2 } }),
	]);
	assert.deepEqual(uptake.toolsByName, { rlm: 4, bash: 3 });
	assert.equal(uptake.rlmRunShare, 2 / 3);
	assert.deepEqual(uptake.framesSpawned, { runs: 2, total: 2, median: 1 });
	assert.deepEqual(uptake.rootUnseenBytes, { runs: 1, total: 1000, median: 1000 });
	assert.deepEqual(summarizeUptake([run({})]).rootUnseenBytes, { runs: 0, total: null, median: null });
});

test("judge scores are reported next to the deterministic pass rate and never change the gate", () => {
	const judgement = (normalized) => ({ normalized, passed: normalized >= 0.7 });
	const records = [
		run({ variant: "pi", judge: judgement(0.9) }),
		run({ variant: "pi", judge: judgement(0.8) }),
		// The candidate passes every deterministic check but the judge dislikes the work.
		run({ variant: "ultron", judge: judgement(0.1) }),
		run({ variant: "ultron", judge: { error: "no JSON object in the reply" } }),
	];
	const summary = summarize(records, ["pi", "ultron"]);
	assert.equal(summary.byVariant.ultron.passRate, 1);
	assert.equal(summary.byVariant.ultron.judged.meanScore, 0.1);
	assert.equal(summary.byVariant.ultron.judged.errors, 1);
	assert.equal(summary.byVariant.pi.judged.judgePassRate, 1);
	assert.ok(summary.gate.every((entry) => entry.check !== "judge"));
	assert.equal(summary.passed, true);
	const withoutJudge = summarize(
		records.map(({ judge, ...rest }) => rest),
		["pi", "ultron"],
	);
	assert.deepEqual(withoutJudge.gate, summary.gate);
	assert.equal(summarizeJudged(records.filter((record) => !record.judge)), null);
});
