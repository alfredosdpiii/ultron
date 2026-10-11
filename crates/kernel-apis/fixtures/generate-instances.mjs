// Generates crates/kernel-apis/fixtures/instances.json: `instances.*` cells through runtime.py + kernel.ts + NativeRlmHost
// with createInstanceModule (ultron/instances.ts) served the scripted task API of fake-tasks.mjs (two lane pins), plus
// a restart: a fresh module over the stored document re-pins every open instance.
// Usage: node crates/kernel-apis/fixtures/generate-instances.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fakeTasks } from "./fake-tasks.mjs";
import { attempt, FIXED_NOW, scenario, show } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const { createInstanceModule } = await import(pathToFileURL(join(pkg, "src/ultron/instances.ts")).href);
const { createMemoryModuleStore } = await import(pathToFileURL(join(pkg, "src/ultron/rlm/host-module.ts")).href);

function instancesHost() {
	const store = createMemoryModuleStore();
	const fake = fakeTasks(FIXED_NOW);
	return {
		modules: [fake.wrap(createInstanceModule({ store })), fake.testModule],
		state: async () => {
			const document = (await store.read()) ?? null;
			// Restart: a new owner over the same document re-pins the open instances.
			const restarted = fakeTasks(FIXED_NOW);
			await restarted.wrap(createInstanceModule({ store: createMemoryModuleStore(document ?? undefined) })).start();
			return { document, log: fake.log, restart: restarted.log };
		},
	};
}

const clock = (ms) => `await rlm.host_request("test.clock", {"ms": ${ms}})`;
const caller = (id) => `await rlm.host_request("test.caller", {"id": ${id === null ? "None" : JSON.stringify(id)}})`;
const finish = (id) => `await rlm.host_request("test.finish", {"id": ${JSON.stringify(id)}, "value": "done"})`;
const attempts = (calls) => `import json as _json\n${calls.map(attempt).join("\n")}`;
const first = "i1 = (await instances.list())[0]['id']";

const scenarios = [
	[
		"retain, invoke, close",
		[
			show("await instances.list()"),
			show('await instances.retain("task-done")'),
			`${clock(1000)}\n${show('await instances.retain("task-done")')}`,
			attempts(['instances.retain("child-done")', 'instances.retain("nope")', 'instances.retain("task-a")', 'instances.retain("det-done")', 'instances.retain("task-failed")', 'instances.retain("")', 'rlm.host_request("instances.retain", {"task_id": "task-done", "x": 1})']),
			`${caller("task-a")}\n${show('[await instances.retain("child-done"), await instances.retain("other-done")]')}`,
			`${caller("task-a")}\n${first}\n${show("await instances.list()")}\n${attempts(["instances.get(i1)", "instances.close(i1)", 'instances.invoke(i1, {"q": 1})'])}`,
			`${caller(null)}\n${first}\n${show('await instances.invoke(i1, {"q": 1})')}\n${attempts(['instances.invoke(i1, {"q": 2})'])}`,
			`${first}\n${finish("spawned-1")}\n${clock(500)}\n${show('[await instances.invoke(i1, ["a", 2], key="k1"), await instances.get(i1)]')}`,
			`${first}\n${finish("spawned-2")}\n${show('[await instances.invoke(i1, ["a", 2], key="k1"), await instances.get(i1)]')}`,
			`${first}\n${attempts(['instances.invoke(i1, 1, key="")', 'rlm.host_request("instances.invoke", {"id": i1})', 'instances.invoke("", 1)', 'instances.invoke("nope", 1)', 'rlm.host_request("instances.invoke", {"id": i1, "input": 1, "extra": 2})'])}`,
			`${first}\n${clock(2000)}\n${show("[await instances.close(i1), await instances.close(i1)]")}\n${attempts(["instances.invoke(i1, 1)", 'instances.get("nope")', "rlm.host_request(\"instances.list\", {\"x\": 1})", 'rlm.host_request("instances.nope", {})'])}`,
			show('[await instances.retain("task-done"), await instances.list()]'),
			`${caller("task-a")}\n${show('await instances.retain("other-done")')}`,
		],
	],
];

const recorded = [];
for (const [name, cells] of scenarios) recorded.push(await scenario(name, instancesHost, cells));
writeFileSync(join(here, "instances.json"), `${JSON.stringify({ area: "instances", scenarios: recorded }, null, "\t")}\n`);
console.log(`instances: ${recorded.length} scenarios`);
