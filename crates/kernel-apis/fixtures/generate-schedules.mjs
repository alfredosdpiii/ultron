// Generates crates/kernel-apis/fixtures/schedules.json: cells calling `schedules.*` and `goals.*` (schedules_api.py)
// through the real runtime.py and kernel.ts, served by NativeRlmHost with the schedule module of
// ultron/schedules.ts. The module sees a scripted task API (fakeTasks below; tests/schedules.rs has the same one):
// sequential task ids, idempotent keys, results scripted by definition, a clock advanced by `test.clock`, and a
// call log. State: the module document and the call log. Usage: node crates/kernel-apis/fixtures/generate-schedules.mjs
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { attempt, FIXED_NOW, record, show } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { createMemoryModuleStore } = await load("src/ultron/rlm/host-module.ts");
const { createScheduleModule } = await load("src/ultron/schedules.ts");

/**
 * Scripted NativeHostApi: spawn refuses "refused-check@1", otherwise reuses the task of an existing key or admits
 * task-<n>; result is scripted by definition. Every call is logged.
 */
export function fakeTasks(clock, preloaded = []) {
	const tasks = preloaded.map((task) => ({ ...task }));
	const log = [];
	const results = {
		"pass-check@1": { status: "succeeded", value: { passed: true } },
		"false-check@1": { status: "succeeded", value: { passed: false } },
		"vague-check@1": { status: "succeeded", value: "fine" },
		"crash-check@1": { status: "failed", error: "check crashed" },
	};
	return {
		log,
		api: {
			callerTaskId: () => null,
			now: () => clock.now,
			tasks: async () => {
				log.push(["tasks"]);
				return tasks.map((task) => ({ ...task }));
			},
			spawn: async (request, parent) => {
				log.push(["spawn", structuredClone(request), parent]);
				if (request.definition === "refused-check@1") throw new Error("Usage budget exhausted");
				const existing = tasks.find((task) => task.key === request.key);
				if (existing) return { ...existing };
				const task = { id: `task-${tasks.length + 1}`, key: request.key, definition: request.definition, state: "succeeded" };
				tasks.push(task);
				return { ...task };
			},
			result: async (id) => {
				log.push(["result", id]);
				const task = tasks.find((item) => item.id === id);
				if (!task) throw new Error("Unknown Ultron task");
				if (task.definition === "lost-check@1") throw new Error("result unavailable");
				return structuredClone(results[task.definition] ?? { status: "succeeded", value: null });
			},
		},
	};
}

function schedulesHost(preloaded) {
	return () => {
		const store = createMemoryModuleStore(preloaded?.document);
		const clock = { now: FIXED_NOW };
		const fake = fakeTasks(clock, preloaded?.tasks ?? []);
		const module = createScheduleModule({ store, tickIntervalMs: 0 });
		const wrapped = {
			prefixes: module.prefixes,
			start: () => module.start(fake.api),
			handle: (request) => module.handle(request, fake.api),
		};
		const ticker = { prefixes: ["test.clock"], handle: async (request) => ((clock.now += request.payload.ms), clock.now) };
		return { modules: [wrapped, ticker], state: async () => ({ document: (await store.read()) ?? null, log: fake.log }) };
	};
}

const pre = (lines) => `import json as _json\n${lines.join("\n")}`;
const tick = (ms) => `await rlm.host_request('test.clock', {'ms': ${ms}})`;

const scenarios = [
	[
		"create, tick, skip, max_runs, pause, delete",
		[
			show("await schedules.list()"),
			show('await schedules.create("report@1", {"q": 1}, 1000, max_runs=3, model="openai/gpt-x")'),
			show("await schedules.tick()"),
			show("await schedules.tick()"),
			pre([tick(999), "print(_json.dumps(await schedules.tick(), sort_keys=True))", tick(1), "print(_json.dumps(await schedules.tick(), sort_keys=True))"]),
			pre([tick(3500), "print(_json.dumps(await schedules.tick(), sort_keys=True))", tick(5000), "print(_json.dumps(await schedules.tick(), sort_keys=True))"]),
			pre([
				"s = await schedules.create('daily@2', [], 500, start_at=" + (FIXED_NOW + 20_000) + ")",
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				"print(_json.dumps(await schedules.pause(s['id']), sort_keys=True))",
				tick(30_000),
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				"print(_json.dumps(await schedules.resume(s['id']), sort_keys=True))",
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				"print(_json.dumps(await schedules.delete(s['id']), sort_keys=True))",
			]),
			pre([
				"done = [s for s in await schedules.list() if s['state'] == 'completed'][0]",
				attempt("schedules.pause(done['id'])"),
				attempt("schedules.resume(done['id'])"),
				attempt("schedules.delete('nope')"),
				attempt("schedules.pause('')"),
				attempt("schedules.create('Bad', {}, 1000)"),
				attempt("schedules.create('a@1', {}, 0)"),
				attempt("schedules.create('a@1', {}, 1.5)"),
				attempt("schedules.create('a@1', {}, 366 * 24 * 3600 * 1000 + 1)"),
				attempt("schedules.create('a@1', None, 1000)"),
				attempt("schedules.create('a@1', {}, 1000, start_at=-1)"),
				attempt("schedules.create('a@1', {}, 1000, max_runs=0)"),
				attempt("schedules.create('a@1', {}, 1000, model='gpt')"),
				attempt("schedules.create('a@1', {}, 1000, model='a/ b')"),
				attempt("schedules.create('a@1', {}, 1000, goal_id='goal-x')"),
				attempt("rlm.host_request('schedules.create', {'definition': 'a@1', 'input': 1, 'every_ms': 5, 'owner': 'x'})"),
				attempt("rlm.host_request('schedules.list', {'x': 1})"),
				attempt("rlm.host_request('schedules.tick', {'x': 1})"),
				attempt("rlm.host_request('schedules.nope', {})"),
				attempt("rlm.host_request('goals.nope', {})"),
			]),
			show("await schedules.list()"),
		],
	],
	[
		"goals: verify, caps, attach, pause",
		[
			show('await goals.create("Ship it", ["pass-check@1", "false-check@1"])'),
			pre([
				"g = (await goals.list())[0]",
				"print(_json.dumps(await goals.verify(g['id']), sort_keys=True))",
				"print(_json.dumps(await goals.verify(g['id'], input={'round': 2}), sort_keys=True))",
			]),
			pre([
				"g = await goals.create('Pass', ['pass-check@1'], max_tasks=3)",
				"s = await schedules.create('work@1', {'n': 1}, 1000, goal_id=g['id'])",
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				"print(_json.dumps(await goals.pause(g['id']), sort_keys=True))",
				tick(1000),
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				attempt("goals.verify(g['id'])"),
				"print(_json.dumps(await goals.resume(g['id']), sort_keys=True))",
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				"print(_json.dumps(await goals.verify(g['id']), sort_keys=True))",
				attempt("goals.verify(g['id'])"),
				attempt("goals.pause(g['id'])"),
				attempt("schedules.create('work@1', {}, 1000, goal_id=g['id'])"),
				tick(1000),
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
			]),
			pre([
				"g = await goals.create('Capped', ['pass-check@1', 'pass-check@2'], max_tasks=1)",
				attempt("goals.verify(g['id'])"),
				"s = await schedules.create('work@1', {}, 100, goal_id=g['id'])",
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				tick(100),
				"print(_json.dumps(await schedules.tick(), sort_keys=True))",
				attempt("goals.attach(g['id'], 'task-1')"),
				attempt("goals.attach(g['id'], 'nope')"),
				attempt("goals.attach('goal-nope', 'task-1')"),
				"await schedules.delete(s['id'])",
			]),
			pre([
				"g = await goals.create('Mixed', ['vague-check@1', 'crash-check@1', 'refused-check@1', 'lost-check@1'])",
				"print(_json.dumps(await goals.verify(g['id']), sort_keys=True))",
				"e = await goals.create('Empty', [])",
				"print(_json.dumps(await goals.verify(e['id']), sort_keys=True))",
				"a = await goals.create('Attach', [], max_tasks=0)",
				attempt("goals.attach(a['id'], 'task-1')"),
				"b = await goals.create('Attach2', [])",
				"print(_json.dumps(await goals.attach(b['id'], 'task-1'), sort_keys=True))",
				"print(_json.dumps(await goals.attach(b['id'], 'task-1'), sort_keys=True))",
			]),
			pre([
				attempt("goals.create('', [])"),
				attempt("goals.create('t', 'x')"),
				attempt("goals.create('t', ['bad'])"),
				attempt("goals.create('t', ['a@1', 'a@1'])"),
				attempt("goals.create('t', [], max_tasks=-1)"),
				attempt("goals.get('nope')"),
				attempt("goals.get('')"),
				attempt("goals.verify('nope')"),
				attempt("rlm.host_request('goals.verify', {'id': 'x', 'extra': 1})"),
				attempt("rlm.host_request('goals.get', {'id': 'x', 'extra': 1})"),
				attempt("rlm.host_request('goals.list', {'x': 1})"),
			]),
			show("await goals.list()"),
		],
	],
	[
		"restart: pending slot resumes, goal links reconcile",
		[
			show("await schedules.tick()"),
			show("await goals.list()"),
			show("await schedules.list()"),
		],
	],
];

// A document left by a crash: a claimed-but-unadmitted slot on a goal schedule, and an admitted task the goal does
// not list yet (plus a verify task keyed for the goal).
const restart = {
	document: {
		schedules: [
			{
				id: "schedule-a", definition: "work@1", input: { n: 1 }, every_ms: 1000, start_at: FIXED_NOW - 5000, max_runs: null,
				goal_id: "goal-a", model: null, owner: null, state: "active", created_at: FIXED_NOW - 5000, runs: 2, last_slot: 4,
				pending_slot: 4, last_task_id: "task-1", last_fired_at: FIXED_NOW - 2000, last_skipped: 1, skipped_total: 1,
				last_failure: null, failures: 0,
			},
		],
		goals: [
			{
				id: "goal-a", title: "Recover", required_checks: ["pass-check@1"], max_tasks: null, owner: null, state: "active",
				created_at: FIXED_NOW - 5000, task_ids: ["task-1"], verify_rounds: 1, verification: null,
			},
		],
	},
	tasks: [
		{ id: "task-1", key: "schedule:schedule-a:2", definition: "work@1", state: "succeeded" },
		{ id: "task-2", key: "goal:goal-a:verify:1:0", definition: "pass-check@1", state: "succeeded" },
		{ id: "task-3", key: "schedule:schedule-a:4", definition: "work@1", state: "succeeded" },
	],
};

const areas = {
	schedules: [
		(dir) => schedulesHost(undefined)(dir),
		scenarios.slice(0, 2),
	],
	"schedules-restart": [(dir) => schedulesHost(restart)(dir), scenarios.slice(2)],
};

// One fixture file: record both areas, then merge.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	const { readFileSync, writeFileSync, rmSync } = await import("node:fs");
	await record(areas);
	const main = JSON.parse(readFileSync(join(here, "schedules.json"), "utf8"));
	const second = JSON.parse(readFileSync(join(here, "schedules-restart.json"), "utf8"));
	rmSync(join(here, "schedules-restart.json"));
	main.restart = { preloaded: restart, scenarios: second.scenarios };
	writeFileSync(join(here, "schedules.json"), `${JSON.stringify(main, null, "\t")}\n`);
}
