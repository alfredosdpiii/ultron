// Generates crates/kernel-apis/fixtures/goal.json: the session goal (ultron/goal.ts GoalDriver) served by the TS host
// (NativeRlmHost with the driver's `goal.*` module) to the real runtime.py through kernel.ts. The worker side is
// injected deterministically: runCheck returns scripted results per command, startJob hands out task-1, task-2, ...
// (and refuses an objective containing "unstartable"), stopJob is recorded; the clock is fixed. Cells drive the
// person's `/goal` command, the worker's task-end observer and requests from other lanes through a `test.goal.*`
// module. State: the goal document and the injected calls. Goal ids (goal-<8 hex>) are normalized to goal-ID.
// The `pure` section records checkSignature / goalBrief / goalStatusText over a table.
// Usage: node crates/kernel-apis/fixtures/generate-goal.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { FIXED_NOW, scenario, show, attempt } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { GoalDriver, checkSignature, goalBrief, goalStatusText } = await load("src/ultron/goal.ts");
const { createMemoryModuleStore } = await load("src/ultron/rlm/host-module.ts");

const FAIL = { exit_code: 1, timed_out: false, output: "1 failed in 0.31s" };
const PASS = { exit_code: 0, timed_out: false, output: "3 passed in 0.12s" };
/** Scripted check results per command; the last one repeats. */
export const CHECKS = {
	"pytest -q": [FAIL, FAIL, { exit_code: 1, timed_out: false, output: "2 failed in 0.40s" }, PASS],
	bench: [...Array(10).fill({ exit_code: 1, timed_out: false, output: "slow: 812ms at 0x7f3a" }), { exit_code: 1, timed_out: false, output: "slow: 640ms" }],
	"long-output": [{ exit_code: 2, timed_out: false, output: `${"y".repeat(2500)}é end` }],
	hang: [{ exit_code: null, timed_out: true, output: "" }],
};

function goalHost() {
	const store = createMemoryModuleStore();
	const calls = [];
	const used = {};
	let jobs = 0;
	const driver = new GoalDriver({
		store,
		runCheck: async (command, timeoutSeconds) => {
			calls.push(["check", command, timeoutSeconds]);
			const list = CHECKS[command] ?? [PASS];
			const index = Math.min(used[command] ?? 0, list.length - 1);
			used[command] = (used[command] ?? 0) + 1;
			return structuredClone(list[index]);
		},
		startJob: async (prompt, options) => {
			calls.push(["start", options.key, options.usageRoot, prompt]);
			if (prompt.includes("unstartable")) throw new Error("no capacity for a background job");
			jobs += 1;
			return `task-${jobs}`;
		},
		stopJob: async (id, reason) => {
			calls.push(["stop", id, reason]);
		},
		laneOfJob: (id) => `ultron.background-job.${id}`,
		now: () => FIXED_NOW,
	});
	const test = {
		prefixes: ["test.goal."],
		handle: async (request) => {
			const { payload } = request;
			if (request.type === "test.goal.command") return { text: await driver.command(payload.args) };
			if (request.type === "test.goal.ended") {
				driver.taskEnded({ id: payload.id, result: payload.result });
				await driver.settled();
				return null;
			}
			if (request.type === "test.goal.as")
				return driver.module.handle({ type: payload.type, payload: payload.payload, caller: { lane: payload.lane }, context: request.context }, {});
			throw new Error(`unknown ${request.type}`);
		},
	};
	return {
		modules: [driver.module, test],
		state: async () => {
			await driver.settled();
			return { document: (await store.read()) ?? null, calls };
		},
	};
}

const cmd = (args) => `print((await rlm.host_request("test.goal.command", {"args": ${JSON.stringify(args)}}))["text"])`;
const as = (lane, type, payload = "{}") => `rlm.host_request("test.goal.as", {"lane": ${JSON.stringify(lane)}, "type": "${type}", "payload": ${payload}})`;
const ended = (id, result) => `await rlm.host_request("test.goal.ended", {"id": "${id}", "result": ${result}})`;
const tries = (...calls) => `import json as _json\n${calls.map(attempt).join("\n")}`;

const scenarios = [
	[
		"command, check, complete, block",
		[
			show("await goal.get()"),
			tries("goal.check()", "goal.complete(1, 's', ['e'])", "goal.blocked(1, 'r')"),
			cmd(""),
			cmd("  ship the parser  "),
			show("await goal.get()"),
			tries("goal.check()"),
			cmd("check pytest -q"),
			tries(
				"goal.complete(1, 's', ['e'])",
				"goal.complete('2', 's', ['e'])",
				"goal.complete(2, '  ', ['e'])",
				"goal.complete(2, 's', [])",
				"goal.complete(2, 's', 'e')",
				"goal.complete(2, 's', ['ok', ' '])",
				"goal.complete(2, 's', ['pytest: exit 0'])",
				"goal.blocked(2, 'no access')",
				"goal.check()",
				"goal.blocked(2, '')",
				"goal.blocked(2, 'no access to the CI runner')",
				"goal.complete(2, 's', ['e'])",
				"goal.check()",
			),
			cmd("status"),
			cmd("resume"),
			cmd("pause"),
			cmd("pause"),
			cmd("resume"),
			tries(
				`${as("ultron.background-job.task-1", "goal.check")}`,
				`${as("ultron.child.x", "goal.get")}`,
				`${as("ultron.background-job.task-3", "goal.complete", '{"revision": 2, "summary": "parser shipped", "evidence": ["pytest -q: exit 0", "src/parser.py:1-80"]}')}`,
			),
			cmd("resume"),
			cmd(""),
			tries('rlm.host_request("goal.get", {"x": 1})', 'rlm.host_request("goal.check", {"x": 1})', 'rlm.host_request("goal.nope", {})'),
			"print(repr(goal))",
			cmd("clear"),
			cmd("clear"),
			cmd("pause"),
			cmd("resume"),
			cmd("check x"),
			show("await goal.get()"),
		],
	],
	[
		"stuck checks, task ends, replacement",
		[
			cmd("make it fast"),
			cmd("check bench"),
			"import json as _json\nfor i in range(10):\n    r = await goal.check()\n    print(i, r['passed'], r['paused'], r.get('reason'))\nprint(_json.dumps(r, sort_keys=True))",
			"import time\ntime.sleep(0.3)",
			show("await goal.get()"),
			cmd("resume"),
			ended("task-99", '{"status": "succeeded", "value": "other"}'),
			ended("task-2", '{"status": "succeeded", "value": "no luck"}'),
			show("await goal.get()"),
			cmd("resume"),
			show("await goal.check()"),
			ended("task-3", '{"status": "succeeded", "value": "' + "z".repeat(1200) + '"}'),
			show("await goal.get()"),
			cmd(""),
			ended("task-4", '{"status": "failed", "error": {"message": "boom"}}'),
			cmd(""),
			cmd("check"),
			cmd("resume"),
			tries("goal.blocked((await goal.get())['revision'], 'the API is down')"),
			cmd("resume"),
			cmd("check long-output"),
			show("await goal.check()"),
			cmd("check hang"),
			show("await goal.check()"),
			cmd("second goal " + "x".repeat(4100)),
			"g = await goal.get()\nprint(g['revision'], len(g['objective']), g['status'], g['runs'])",
			cmd("unstartable goal"),
			cmd("resume"),
			show("await goal.get()"),
		],
	],
];

const goalOf = (fields) => ({
	id: "goal-1234abcd",
	revision: 3,
	objective: "ship it",
	check: null,
	status: "active",
	reason: null,
	job: null,
	runs: 0,
	checks: 0,
	stuck: 0,
	last_signature: null,
	run_progress: false,
	last_check: null,
	last_answer: null,
	created_at: 0,
	updated_at: 0,
	completion: null,
	...fields,
});
const signatures = [
	FAIL,
	PASS,
	{ ...FAIL, output: "1 failed in 0.31s at 0x7f00" },
	{ ...FAIL, output: "1 failed in 12.5s at 0x1234" },
	{ ...FAIL, output: "1 failed" },
	{ ...FAIL, output: "2 failed" },
	{ ...FAIL, exit_code: 2 },
	{ exit_code: null, timed_out: true, output: "" },
	{ exit_code: 0, timed_out: true, output: "x" },
	{ exit_code: 1, timed_out: false, output: "  a\t\n b 5 seconds 3sec 4secs 7 ms 9msx 0XFF " },
	{ exit_code: 1, timed_out: false, output: `${"q".repeat(2100)} tail` },
	{ exit_code: 1, timed_out: false, output: "x5s 5s_ é 5s" },
].map((result) => ({ result, signature: checkSignature(result) }));
const goals = [
	null,
	goalOf({}),
	goalOf({ check: "npm test", last_answer: "tried caching", status: "paused", reason: "paused by you", job: "task-7", runs: 2, checks: 4, stuck: 3 }),
	goalOf({ check: "npm test", stuck: 1, last_check: { command: "npm test", passed: false, exit_code: 1, timed_out: false, output: "", at: 0 } }),
	goalOf({ check: "npm test", last_check: { command: "npm test", passed: false, exit_code: null, timed_out: true, output: "", at: 0 } }),
	goalOf({ status: "complete", last_check: { command: "c", passed: true, exit_code: 0, timed_out: false, output: "", at: 0 }, completion: { summary: "done", evidence: ["e"], verified: true, at: 0 } }),
	goalOf({ status: "complete", completion: { summary: "done anyway", evidence: ["e"], verified: false, at: 0 } }),
];
const pure = {
	signatures,
	briefs: goals.filter(Boolean).map((goal) => ({ goal, brief: goalBrief(goal) })),
	statuses: goals.map((goal) => ({ goal, text: goalStatusText(goal) })),
};

const recorded = [];
for (const [name, cells] of scenarios) recorded.push(await scenario(name, goalHost, cells));
const text = JSON.stringify({ area: "goal", scenarios: recorded, pure }, null, "\t").replace(/goal-[0-9a-f]{8}/g, "goal-ID");
writeFileSync(join(here, "goal.json"), `${text}\n`);
console.log(`goal: ${recorded.length} scenarios`);
