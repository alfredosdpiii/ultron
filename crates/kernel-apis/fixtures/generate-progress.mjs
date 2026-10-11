// Generates crates/kernel-apis/fixtures/progress.json: `progress.*` cells through runtime.py + kernel.ts + NativeRlmHost
// with createProgressModule (ultron/progress.ts) served the scripted task API of fake-tasks.mjs, plus a table of
// stored documents reloaded by a fresh module (pending verifier claims settle as unverified; invalid documents fail).
// Usage: node crates/kernel-apis/fixtures/generate-progress.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fakeTasks } from "./fake-tasks.mjs";
import { attempt, FIXED_NOW, scenario, show } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const { createProgressModule } = await import(pathToFileURL(join(pkg, "src/ultron/progress.ts")).href);
const { createMemoryModuleStore } = await import(pathToFileURL(join(pkg, "src/ultron/rlm/host-module.ts")).href);

function progressHost() {
	const store = createMemoryModuleStore();
	const fake = fakeTasks(FIXED_NOW);
	return {
		modules: [fake.wrap(createProgressModule({ store })), fake.testModule],
		state: async () => ({ document: (await store.read()) ?? null, log: fake.log }),
	};
}

const A = "a".repeat(64);
const B = "b".repeat(64);
const clock = (ms) => `await rlm.host_request("test.clock", {"ms": ${ms}})`;
const caller = (id) => `await rlm.host_request("test.caller", {"id": ${id === null ? "None" : JSON.stringify(id)}})`;
const attempts = (calls) => `import json as _json\n${calls.map(attempt).join("\n")}`;
const MIN = 60_000;

const scenarios = [
	[
		"report, assess, stall",
		[
			show('await progress.report("started", [{"kind": "file", "ref": "a.py"}], task_id="task-a")'),
			show('await progress.assess("task-a")'),
			show('[await progress.report("again", [{"kind": "file", "ref": "a.py"}], task_id="task-a"), await progress.report("hashed", [{"kind": "file", "ref": "a.py", "sha256": "' + A + '"}], task_id="task-a"), await progress.report("same hash", [{"kind": "file", "ref": "a.py", "sha256": "' + A + '"}, {"kind": "test", "ref": "t1", "sha256": None}], task_id="task-a"), await progress.report("new hash", [{"kind": "file", "ref": "a.py", "sha256": "' + B + '"}, {"kind": "file", "ref": "a.py"}], task_id="task-a", metrics={"tests": 3, "ratio": 0.5})]'),
			`${clock(11 * MIN)}\n${show('[await progress.report("repeat", [{"kind": "file", "ref": "a.py"}], task_id="task-a"), await progress.assess("task-a")]')}`,
			`${clock(10 * MIN)}\n${show('await progress.assess("task-a")')}`,
			`${clock(MIN)}\n${show('await progress.assess("task-a")')}`,
			`import json as _json\nfor i in range(256):\n    await progress.report("tick", [{"kind": "tick", "ref": "r"}], task_id="task-idle")\n${attempt('progress.report("one more", [{"kind": "tick", "ref": "r"}], task_id="task-idle")')}`,
			show('[await progress.assess("task-idle"), await progress.assess("task-done"), await progress.assess("task-failed")]'),
			attempts([
				'progress.report("x", [{"kind": "file", "ref": "a"}])',
				'progress.report("", [{"kind": "file", "ref": "a"}], task_id="task-a")',
				'progress.report("x" * 4097, [{"kind": "file", "ref": "a"}], task_id="task-a")',
				'progress.report("x", [], task_id="task-a")',
				'progress.report("x", [{"kind": "f", "ref": str(i)} for i in range(33)], task_id="task-a")',
				'progress.report("x", ["a"], task_id="task-a")',
				'progress.report("x", [{"kind": "File", "ref": "a"}], task_id="task-a")',
				'progress.report("x", [{"kind": "file", "ref": " "}], task_id="task-a")',
				'progress.report("x", [{"kind": "file", "ref": "r" * 1025}], task_id="task-a")',
				'progress.report("x", [{"kind": "file", "ref": "a", "sha256": "A" * 64}], task_id="task-a")',
				'progress.report("x", [{"kind": "file", "ref": "a", "extra": 1}], task_id="task-a")',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id="task-a", metrics=[1])',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id="task-a", metrics={"": 1})',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id="task-a", metrics={"m": "x"})',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id="task-a", metrics={str(i): i for i in range(33)})',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id="nope")',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id="task-b")',
				'progress.report("x", [{"kind": "file", "ref": "a"}], task_id=" ")',
				'progress.assess("")',
				'progress.assess("nope")',
				'rlm.host_request("progress.report", {"summary": "x", "evidence": [], "who": 1})',
				'rlm.host_request("progress.assess", {"task_id": "task-a", "x": 1})',
				'rlm.host_request("progress.nope", {})',
			]),
		],
	],
	[
		"callers",
		[
			`${caller("task-a")}\n${show('[await progress.report("self", [{"kind": "log", "ref": "l1"}]), await progress.report("child", [{"kind": "log", "ref": "l2"}], task_id="task-b")]')}`,
			attempts(['progress.report("grandchild", [{"kind": "log", "ref": "l3"}], task_id="task-c")', 'progress.assess("task-c")', 'progress.history("task-b")', 'progress.assess("task-idle")']),
			`${caller("task-b")}\n${attempts(['progress.assess("task-a")', 'progress.history("task-a")', 'progress.reassess("task-a")', 'progress.assess("task-b")', 'progress.report("up", [{"kind": "log", "ref": "x"}], task_id="task-a")'])}`,
			`${caller(null)}\n${show('await progress.history("task-a")')}`,
		],
	],
	[
		"reassess and claims",
		[
			show('await progress.report("built", [{"kind": "build", "ref": "out/app", "sha256": "' + A + '"}], task_id="task-done")'),
			show('[await progress.reassess("task-a"), await progress.reassess("task-a", extend_budget=True), await progress.reassess("task-a", extend_budget={"tokens": 100}), await progress.reassess("task-a", extend_budget=False)]'),
			show('[await progress.reassess("task-a", claim="complete"), await progress.reassess("task-failed", claim="complete"), await progress.reassess("task-done", claim="complete")]'),
			show('await progress.reassess("task-done", claim="complete", verifier="verify-pass@1")'),
			show('[await progress.reassess("task-done", claim="complete", verifier=v) for v in ["verify-fail@1", "verify-odd@1", "verify-error@1", "verify-nostart@1", "verify-noresult@1"]]'),
			show('await progress.reassess("task-done", claim="complete", verifier="verify-pass@1", verifier_input={"x": 1}, extend_budget={"calls": 2})'),
			attempts([
				'progress.reassess("task-done", claim="done")',
				'progress.reassess("task-done", claim="complete", verifier="bad")',
				'progress.reassess("task-done", claim="complete", verifier="Verify@1")',
				'progress.reassess("task-done", verifier="verify-pass@1")',
				'progress.reassess("task-done", verifier_input={"x": 1})',
				'progress.reassess("task-done", extend_budget="yes")',
				'progress.reassess("task-done", extend_budget=[1])',
				'progress.reassess("nope")',
				'progress.reassess("")',
				'rlm.host_request("progress.reassess", {"task_id": "task-done", "claim": None, "extend_budget": None})',
			]),
			show('await progress.history("task-done")'),
		],
	],
];

const decision = (claim) => ({
	id: "d1",
	task_id: "t1",
	requester_task_id: null,
	at: 5,
	assessment: { classification: "finished", reasons: ["r"], receipts: [] },
	...(claim ? { claim } : {}),
});
const reloadInputs = [
	{ receipts: [], decisions: [decision({ claim: "complete", decision: "pending", reason: "Verifier running", verifier: "v@1" }), decision(null)] },
	{ version: 1, receipts: [], decisions: [decision({ claim: "complete", decision: "verified", reason: "ok" })] },
	{ version: 1, receipts: [{ id: "r", task_id: "t", reporter_task_id: null, at: 1, summary: "s", evidence: [{ kind: "k", ref: "r", sha256: "x" }], metrics: { m: 1 }, new_evidence: [] }], decisions: [] },
	{ version: 1, receipts: [], decisions: [], extra: 1 },
	{ version: 1, receipts: [{ id: "", task_id: "t", reporter_task_id: null, at: 1, summary: "s", evidence: [], new_evidence: [] }], decisions: [] },
	{ version: 1, receipts: [], decisions: [{ ...decision(null), assessment: { classification: "odd", reasons: [], receipts: [] } }] },
	{ version: 1, receipts: [], decisions: [{ ...decision(null), budget: { requested: 1, decision: "granted", reason: "r" } }] },
	{ version: 2, receipts: [], decisions: [] },
	{ version: 1, receipts: {}, decisions: [] },
];
const reload = [];
for (const input of reloadInputs) {
	const store = createMemoryModuleStore(input);
	const module = createProgressModule({ store, now: () => FIXED_NOW });
	try {
		await module.start({ now: () => FIXED_NOW });
		reload.push({ input, document: (await store.read()) ?? null });
	} catch (error) {
		reload.push({ input, error: error.message });
	}
}

const recorded = [];
for (const [name, cells] of scenarios) recorded.push(await scenario(name, progressHost, cells));
writeFileSync(join(here, "progress.json"), `${JSON.stringify({ area: "progress", scenarios: recorded, reload }, null, "\t")}\n`);
console.log(`progress: ${recorded.length} scenarios, ${reload.length} reloads`);
