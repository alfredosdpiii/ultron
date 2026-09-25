import { fileURLToPath } from "node:url";
import type { JsonValue } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@earendil-works/chord/context";
import { describe, expect, test } from "vitest";
import type { MemoryBackend } from "../src/ultron/memory.ts";
import { createProgressModule } from "../src/ultron/progress.ts";
import { createReleaseGateModule } from "../src/ultron/release-gate.ts";
import { createMemoryModuleStore, type NativeHostModule } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { createScheduleModule } from "../src/ultron/schedules.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { createWorkerServices } from "../src/ultron/worker-services.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

function durable() {
	let document: JsonValue | undefined;
	return {
		get document() {
			return structuredClone(document);
		},
		store: () => ({
			read: async () => structuredClone(document),
			write: async (next: JsonValue) => {
				document = structuredClone(next);
			},
		}),
	};
}

/** Session values whose refinement/artifact namespace is down; memory journal storage still works. */
function sessionValues() {
	const values = new Map<string, JsonValue>();
	const refuse = (namespace: string) => {
		if (namespace === "ultron.local") throw new Error("refinement store unavailable");
	};
	return {
		getValue: async (address: { namespace: string; key: string }) => {
			refuse(address.namespace);
			const value = values.get(`${address.namespace}\0${address.key}`);
			return value === undefined ? undefined : { address, value, seq: 1 };
		},
		setValue: async (address: { namespace: string; key: string }, value: JsonValue) => {
			refuse(address.namespace);
			values.set(`${address.namespace}\0${address.key}`, structuredClone(value));
		},
		scanValues: async (prefix: { namespace: string }) => {
			refuse(prefix.namespace);
			return [];
		},
	};
}

function outageHarness(prompts: string[]) {
	const lane = {
		getActiveTools: async () => [],
		setModel: async () => {},
		abort: async () => ({ ok: true }),
		prompt: async (text: string) => {
			prompts.push(text);
			return { ok: true, value: { status: "completed", tipId: "tip", fromTipId: null } };
		},
		findEntries: async () => [
			{
				id: "tip",
				type: "message",
				message: { role: "assistant", content: [{ type: "text", text: "summary without optional services" }] },
			},
		],
	};
	return { lane: async () => lane };
}

function deterministicDefinition(id: string, output: JsonValue = {}) {
	return {
		id,
		version: "1",
		strategy: "deterministic",
		instructions: `Deterministic ${id}.`,
		inputSchema: {},
		outputSchema: output,
		maxRepairs: 0,
		inputDescription: "Any JSON value",
		outputDescription: "Any JSON value",
	};
}

const cell = `
import json
data = list(range(1, 100001))
answer = sum(x * x for x in data) % 1000003
outcomes = {}
async def attempt(name, pending):
    try:
        outcomes[name] = {"ok": True, "value": await pending}
    except Exception as error:
        outcomes[name] = {"ok": False, "error": str(error)[:300]}

await attempt("jev", jev.triage("route this task"))
await attempt("memory_recall", memory.prepare("prior fixes", task_id="a34-root"))
await attempt("memory_propose", memory.propose("lesson", [{"ref": "task:a34"}]))
await attempt("refinements", refinements.list())
await attempt("skills", rlm.host_request("skills.list", {}))

nodes = [
    {"id": "effect", "definition": "external-effect@1", "input": {"order": "A34-1"}, "key": "wf:A34-1:effect"},
    {"id": "flaky", "definition": "flaky-step@1", "input": {"step": 2}, "dependsOn": ["effect"]},
    {"id": "summary", "definition": "identity@1", "inputFrom": "flaky", "dependsOn": ["flaky"]},
]
first_run = await workflows.run(nodes)
second_run = await workflows.run(nodes)

child = await agents.spawn("rlm-child@1", {"prompt": "summarize the computed answer"})
child_result = await child.result()
unnamed = await progress.reassess(child.id, claim="complete")
outage_verified = await progress.reassess(child.id, claim="complete", verifier="outage-check@1")
goal = await goals.create("ship A34", ["check-pass@1", "outage-check@1"])
goal_after = await goals.verify(goal["id"])
await gates.define("a34-release", [{"name": "unit", "required": True}, {"name": "memory-regression", "required": True}], "a" * 64)
gate = await gates.compare(
    "a34-release",
    {"variant": "base", "fixture_hash": "a" * 64, "results": {"unit": "passed", "memory-regression": "passed"}},
    {"variant": "cand", "fixture_hash": "a" * 64, "results": {"unit": "passed"}},
)
print(json.dumps({
    "answer": answer,
    "outcomes": outcomes,
    "first_run": first_run,
    "second_run": second_run,
    "child": child_result,
    "unnamed": unnamed["claim"],
    "outage_verified": outage_verified["claim"],
    "goal": {"state": goal_after["state"], "status": goal_after["verification"]["status"]},
    "gate": {"decision": gate["decision"], "reasons": gate["reasons"]},
}))
`;

describe("A34 optional-service failure preserves RLM operation without bypassing checks or replaying effects", () => {
	test("combined Jev, Hindsight, refinement, module, and workflow outage in one real RLM kernel run", async () => {
		const effects: string[] = [];
		const backendCalls: string[] = [];
		const jevCalls: string[] = [];
		const prompts: string[] = [];
		const down = (name: string) => async () => {
			jevCalls.push(name);
			throw new Error("Jev unavailable: connect ECONNREFUSED 127.0.0.1:7777");
		};
		const hindsightDown = (name: string) => async () => {
			backendCalls.push(name);
			throw new Error("Hindsight unavailable");
		};
		const backend: MemoryBackend = {
			namespace: "fake://hindsight-down",
			scopeTags: {
				session: ["ultron:session:a34"],
				project: ["ultron:project:a34"],
				global: ["ultron:global:a34"],
			},
			recall: hindsightDown("recall"),
			retain: hindsightDown("retain"),
			get: hindsightDown("get"),
			delete: hindsightDown("delete"),
			operation: hindsightDown("operation"),
		};
		const brokenSkills: NativeHostModule = {
			prefixes: ["skills."],
			handle: async () => {
				throw new Error("skill module unavailable");
			},
		};
		const stores = { tasks: durable(), usage: durable(), definitions: durable() };
		const build = () =>
			new NativeRlmHost(outageHarness(prompts) as never, {} as never, {
				store: stores.tasks.store(),
				definitionStore: stores.definitions.store(),
				usage: new NativeUsageLedger(stores.usage.store()),
				services: createWorkerServices({
					session: sessionValues() as never,
					sessionId: "a34",
					cwd: process.cwd(),
					jev: { triage: down("triage"), memoryGate: down("memoryGate"), memoryPolicy: down("memoryPolicy") },
					backend,
				}),
				refinements: async () => {
					throw new Error("refinement service unavailable");
				},
				deterministic: async ({ definition, input }) => {
					if (definition.id === "external-effect") {
						effects.push(JSON.stringify(input));
						return { receipt: `receipt-${effects.length}` };
					}
					if (definition.id === "flaky-step") throw new Error("workflow step service unavailable");
					if (definition.id === "outage-check") throw new Error("verifier dependency (memory) unavailable");
					if (definition.id === "check-pass") return { passed: true };
					return input;
				},
				modules: [
					createProgressModule({ store: createMemoryModuleStore() }),
					createScheduleModule({ store: createMemoryModuleStore(), tickIntervalMs: 0 }),
					createReleaseGateModule({ store: createMemoryModuleStore() }),
					brokenSkills,
				],
			});
		const host = build();
		for (const id of ["external-effect", "flaky-step", "outage-check", "check-pass"])
			await host.handle("agents.register", { definition: deterministicDefinition(id) }, BACKGROUND_CONTEXT);
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, (type, payload, signal) =>
			host.handle(type, payload, signal ? withAbortSignal(signal, BACKGROUND_CONTEXT) : BACKGROUND_CONTEXT),
		);
		try {
			const execution = await kernel.execute(cell);
			expect(execution.status, execution.stderr + JSON.stringify(execution.error ?? null)).toBe("ok");
			const report = JSON.parse(execution.stdout.trim().split("\n").at(-1)!);

			// Self-contained RLM work completes over the full data with every optional service down.
			let expected = 0n;
			for (let x = 1n; x <= 100000n; x += 1n) expected += x * x;
			expect(report.answer).toBe(Number(expected % 1000003n));

			// Each optional service fails closed and visibly; none is silently treated as success.
			expect(report.outcomes.jev).toMatchObject({ ok: false });
			expect(report.outcomes.jev.error).toContain("Jev unavailable");
			expect(report.outcomes.memory_recall).toMatchObject({ ok: false });
			expect(report.outcomes.memory_propose).toMatchObject({ ok: false });
			expect(report.outcomes.refinements).toMatchObject({ ok: false });
			expect(report.outcomes.refinements.error).toContain("refinement store unavailable");
			expect(report.outcomes.skills).toMatchObject({ ok: false, error: expect.stringContaining("skill module") });
			// A failed gate cannot reach Hindsight at all, so nothing was retained behind the outage.
			expect(backendCalls).toEqual([]);
			expect(jevCalls).toEqual(["triage", "memoryGate", "memoryPolicy"]);

			// The workflow records its failure and skip; the explicit rerun does not repeat the keyed effect.
			for (const run of [report.first_run, report.second_run]) {
				expect(run.effect).toMatchObject({ status: "succeeded", value: { receipt: "receipt-1" } });
				expect(run.flaky).toMatchObject({ status: "failed", error: "workflow step service unavailable" });
				expect(run.summary).toEqual({ status: "skipped", reason: "Dependency did not succeed" });
			}
			expect(effects).toEqual([JSON.stringify({ order: "A34-1" })]);

			// A model-backed child still runs without its refinement service.
			expect(report.child).toMatchObject({ status: "succeeded", value: "summary without optional services" });
			expect(prompts).toEqual(["summarize the computed answer"]);

			// Required verification still refuses success.
			expect(report.unnamed).toMatchObject({ decision: "unverified" });
			expect(report.outage_verified).toMatchObject({
				decision: "unverified",
				verifier_status: "failed",
				reason: expect.stringContaining("verifier dependency (memory) unavailable"),
			});
			expect(report.goal).toEqual({ state: "active", status: "unachieved" });
			expect(report.gate.decision).toBe("blocked");

			// The kernel remains usable after the combined outage.
			expect(await kernel.execute("answer + 1")).toMatchObject({ status: "ok", result: String(report.answer + 1) });
		} finally {
			await kernel.shutdown();
			await host.close();
		}

		// After an owner restart, the same keyed workflow still does not replay the effect.
		const restarted = build();
		const rerun = (await restarted.handle(
			"workflows.run",
			{
				nodes: [
					{ id: "effect", definition: "external-effect@1", input: { order: "A34-1" }, key: "wf:A34-1:effect" },
				],
			},
			BACKGROUND_CONTEXT,
		)) as { effect: { status: string } };
		expect(rerun.effect).toMatchObject({ status: "succeeded" });
		expect(effects).toHaveLength(1);
		// Jev outage calls are accounted as failed calls, not dropped.
		const status = (await restarted.handle("agents.status", {}, BACKGROUND_CONTEXT)) as {
			usage: { usage: { jevCalls: number } };
			tasks: Array<{ definition: string; state: string }>;
		};
		expect(status.usage.usage.jevCalls).toBe(1);
		expect(status.tasks.filter((task) => task.definition === "external-effect@1")).toHaveLength(1);
		await restarted.close();
	});
});
