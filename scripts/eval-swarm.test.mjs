import assert from "node:assert/strict";
import test from "node:test";
import { activeOrder, simulate } from "../evals/quality/fixtures/swarm/codebase.mjs";
import { solutions } from "../evals/quality/tasks-swarm-solutions.mjs";
import { codebase, followUps, route, tasks } from "../evals/quality/tasks-swarm.mjs";
import { countSwarmCalls, formatRunLine, promptStats, summarize } from "./eval-quality.mjs";

/** Words that would name a runtime feature or strategy; the prompts must stay agent-neutral. */
const STRATEGY_WORDS =
	/\b(ultron|rlm|pi|spawn\w*|sub-?agents?|agents?|delegat\w*|parallel\w*|concurren\w*|child\w*|workers?|board|swarm|route|contracts?)\b/i;

test("swarm prompts name no runtime feature or strategy", () => {
	for (const task of tasks())
		for (const prompt of task.prompts) assert.doesNotMatch(prompt, STRATEGY_WORDS, `${task.id}: ${prompt.slice(0, 80)}`);
});

test("the follow-up prompts never contain their answers", () => {
	for (const { prompts, expected } of [followUps(), route()]) {
		const text = prompts.join("\n");
		for (const [key, value] of Object.entries(expected)) {
			if (key === "stages") continue;
			if (value.code) assert.ok(!text.includes(value.code), `${key} code leaks`);
			if (value.constant) assert.ok(!text.includes(value.constant), `${key} constant leaks`);
		}
	}
});

test("the generated codebases are large, with a dense subsystem, and their questions avoid the traps' answers", () => {
	const lines = (files, prefix = "") =>
		Object.entries(files)
			.filter(([path]) => path.startsWith(prefix))
			.reduce((total, [, content]) => total + content.split("\n").length, 0);
	const follow = codebase("follow-ups");
	assert.ok(lines(follow.files) > 45_000);
	assert.ok(lines(follow.files, "app/dispatch/") > 5_000);
	const q = follow.questions.dispatch;
	const model = follow.models.dispatch;
	assert.deepEqual(simulate(model, q.record), q.first);
	assert.notDeepEqual(q.naive, q.first, "ignoring adjust stages must give another answer");
	assert.notEqual(q.constant.constant, q.constant.decoy);
	assert.notDeepEqual(activeOrder(model), model.order, "DISABLED stages are a trap");
	assert.ok(lines(codebase("route").files) > 45_000);
});

test("countSwarmCalls counts the durable APIs in rlm code", () => {
	const code = [
		"h = await rlm.spawn('read app/dispatch', keep=True)",
		"a = await rlm.ask(h, 'q1')",
		"hits = await swarm.route('nonce reuse')",
		"await board.add('T01'); await board.claim(1); await board.update(1, status='done')",
		"await notes.append('x')",
	].join("\n");
	const counts = countSwarmCalls(code);
	assert.equal(counts.keepSpawn, 1);
	assert.equal(counts.ask, 1);
	assert.equal(counts.route, 1);
	assert.equal(counts.boardAdd + counts.boardClaim + counts.boardUpdate, 3);
	assert.equal(counts.notes, 1);
	assert.equal(counts.send, 0);
	countSwarmCalls("await rlm.ask(h, 'q2')", counts);
	assert.equal(counts.ask, 2);
});

test("promptStats sums root input tokens and counts re-reads of the given paths", () => {
	const events = [
		{ type: "message_end", message: { role: "assistant", usage: { input: 100, cacheRead: 900, cacheWrite: 10, output: 50 } } },
		{ type: "message_end", message: { role: "toolResult" } },
		{ type: "tool_execution_start", toolName: "rlm", args: { code: "print(open('app/dispatch/pipeline.py').read())" } },
		{ type: "tool_execution_start", toolName: "rlm", args: { code: "print(1)" } },
	];
	assert.deepEqual(promptStats(events, ["app/dispatch"]), { inputTokens: 1010, rereads: 1 });
	assert.deepEqual(promptStats(events), { inputTokens: 1010, rereads: 0 });
});

test("summaries report a pass rate without optional tasks, and run lines show swarm calls", () => {
	const record = (task, passed, extra = {}) => ({ task, category: "swarm", variant: "ultron", trial: 1, passed, durationMs: 1000, ...extra });
	const summary = summarize([record("follow-ups", true), record("contracts", false, { optional: "contracts" })], ["ultron"]);
	assert.equal(summary.byVariant.ultron.passRate, 0.5);
	assert.equal(summary.byVariant.ultron.passRateRequired, 1);
	assert.deepEqual(summary.byVariant.ultron.optionalTasks, ["contracts (contracts)"]);
	const plain = summarize([record("follow-ups", true)], ["ultron"]);
	assert.equal(plain.byVariant.ultron.passRateRequired, undefined);
	const line = formatRunLine(record("follow-ups", true, { swarmCalls: countSwarmCalls("await rlm.ask(h, 'q')") }));
	assert.match(line, /swarm \{"ask":1\}/);
});

test("every swarm task has a reference solution", () => {
	for (const task of tasks()) assert.ok(solutions[task.id], task.id);
	assert.equal(tasks().find((task) => task.id === "contracts").optional, "contracts");
});
