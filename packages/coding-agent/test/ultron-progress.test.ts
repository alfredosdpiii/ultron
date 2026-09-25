import { afterEach, describe, expect, test } from "vitest";
import { createProgressModule } from "../src/ultron/progress.ts";
import { createMemoryModuleStore, type HostModuleStore } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;
const SHA_A = "a".repeat(64);
const SHA_B = "b".repeat(64);

function memoryStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => value as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

function waitingLane() {
	return {
		findEntries: async () => [],
		getActiveTools: async () => [],
		setModel: async () => {},
		prompt: () => new Promise<never>(() => {}),
		abort: async () => ({ ok: true }),
		steer: async () => ({ ok: true, value: {} }),
	};
}

type Decision = {
	id: string;
	requester_task_id: string | null;
	assessment: { classification: string };
	budget?: { decision: string; reason: string };
	claim?: { decision: string; reason: string; verifier_task_id?: string; verifier_status?: string };
};

const hosts: NativeRlmHost[] = [];

afterEach(async () => {
	for (const host of hosts.splice(0)) await host.close();
});

async function setup(progressStore: HostModuleStore = createMemoryModuleStore()) {
	let time = 1_000_000;
	const clock = {
		now: () => time,
		advance: (ms: number) => {
			time += ms;
		},
	};
	const lane = waitingLane();
	const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
		store: memoryStore(),
		definitionStore: memoryStore(),
		// identity@1 echoes; "failing-check@1" always throws, so its task fails.
		deterministic: async ({ definition, input }) => {
			if (definition.id === "failing-check") throw new Error("check exploded");
			return input;
		},
		modules: [createProgressModule({ store: progressStore, now: clock.now, stallAfterMs: 60_000 })],
	});
	hosts.push(host);
	const call = <T = unknown>(type: string, payload: Record<string, unknown>, laneName?: string) =>
		host.handle(type, payload, context, laneName === undefined ? undefined : { lane: laneName }) as Promise<T>;
	const running = await call<{ id: string }>("background.start", { prompt: "work" });
	await new Promise((resolve) => setTimeout(resolve, 10));
	const workerLane = `ultron.background-job.${running.id}`;
	return { host, call, clock, running: running.id, workerLane };
}

describe("progress module (A44)", () => {
	test("classifies stalled, progressing, busy, and finished from recorded evidence", async () => {
		const { call, clock, running, workerLane } = await setup();

		const never = await call<{ classification: string; reasons: string[] }>("progress.assess", { task_id: running });
		expect(never.classification).toBe("stalled");
		expect(never.reasons[0]).toContain("never reported");

		const first = await call<{ id: string; reporter_task_id: string; new_evidence: string[] }>(
			"progress.report",
			{ summary: "wrote parser", evidence: [{ kind: "file", ref: "src/parser.ts", sha256: SHA_A }] },
			workerLane,
		);
		expect(first.reporter_task_id).toBe(running);
		expect(first.new_evidence).toEqual(["file:src/parser.ts"]);
		const progressing = await call<{ classification: string; receipts: string[] }>("progress.assess", {
			task_id: running,
		});
		expect(progressing).toMatchObject({ classification: "progressing", receipts: [first.id] });

		// Past the window, repeating identical evidence is activity, not progress.
		clock.advance(61_000);
		const repeat = await call<{ id: string; new_evidence: string[] }>(
			"progress.report",
			{ summary: "still on parser", evidence: [{ kind: "file", ref: "src/parser.ts", sha256: SHA_A }] },
			workerLane,
		);
		expect(repeat.new_evidence).toEqual([]);
		const busy = await call<{ classification: string; receipts: string[] }>("progress.assess", { task_id: running });
		expect(busy).toMatchObject({ classification: "busy", receipts: [repeat.id] });

		// A changed hash on the same ref is new evidence.
		const changed = await call<{ new_evidence: string[] }>(
			"progress.report",
			{ summary: "parser rewritten", evidence: [{ kind: "file", ref: "src/parser.ts", sha256: SHA_B }] },
			workerLane,
		);
		expect(changed.new_evidence).toEqual(["file:src/parser.ts"]);
		expect((await call<{ classification: string }>("progress.assess", { task_id: running })).classification).toBe(
			"progressing",
		);

		clock.advance(60_001);
		const stalled = await call<{ classification: string; reasons: string[] }>("progress.assess", {
			task_id: running,
		});
		expect(stalled.classification).toBe("stalled");
		expect(stalled.reasons[0]).toContain("60001ms old");

		const done = await call<{ id: string }>("agents.spawn", { definition: "identity@1", input: { ok: 1 } });
		await call("agents.result", { id: done.id });
		const finished = await call<Record<string, unknown>>("progress.assess", { task_id: done.id });
		expect(finished).toMatchObject({
			classification: "finished",
			task_state: "completed",
			result_status: "succeeded",
			verification: "unverified",
		});
	});

	test("takes the reporter from the lane and rejects reports for unrelated tasks", async () => {
		const { call, running, workerLane } = await setup();
		const child = await call<{ id: string }>("background.start", { prompt: "child" }, workerLane);
		const sibling = await call<{ id: string }>("background.start", { prompt: "sibling" });
		await new Promise((resolve) => setTimeout(resolve, 10));
		const evidence = [{ kind: "log", ref: "run-1" }];

		const forParent = await call<{ task_id: string; reporter_task_id: string }>(
			"progress.report",
			{ task_id: child.id, summary: "child started", evidence },
			workerLane,
		);
		expect(forParent).toMatchObject({ task_id: child.id, reporter_task_id: running });

		await expect(
			call("progress.report", { task_id: sibling.id, summary: "not mine", evidence }, workerLane),
		).rejects.toThrow("itself or its direct children");
		// The root may report for its direct children but not for grandchildren.
		const rootReport = await call<{ reporter_task_id: null }>("progress.report", {
			task_id: running,
			summary: "root view",
			evidence,
		});
		expect(rootReport.reporter_task_id).toBeNull();
		await expect(call("progress.report", { task_id: child.id, summary: "grandchild", evidence })).rejects.toThrow(
			"itself or its direct children",
		);
		await expect(call("progress.report", { summary: "who?", evidence })).rejects.toThrow("must name the task_id");
		// A child cannot read or reassess its parent.
		await expect(call("progress.history", { task_id: running }, `ultron.background-job.${child.id}`)).rejects.toThrow(
			"descendants",
		);
	});

	test("validates payloads strictly and bounds evidence and receipts", async () => {
		const { call, running, workerLane } = await setup();
		await expect(
			call(
				"progress.report",
				{ summary: "x", evidence: [{ kind: "file", ref: "a" }], reporter: "forged" },
				workerLane,
			),
		).rejects.toThrow("Unknown payload field: reporter");
		await expect(
			call("progress.report", { summary: "x", evidence: [{ kind: "file", ref: "a", extra: 1 }] }, workerLane),
		).rejects.toThrow("Unknown payload field: extra");
		await expect(call("progress.report", { summary: "x", evidence: [] }, workerLane)).rejects.toThrow("evidence");
		await expect(
			call("progress.report", { summary: "x", evidence: [{ kind: "file", ref: "a", sha256: "nope" }] }, workerLane),
		).rejects.toThrow("sha256");
		await expect(
			call(
				"progress.report",
				{ summary: "x", evidence: Array.from({ length: 33 }, (_, i) => ({ kind: "file", ref: `f${i}` })) },
				workerLane,
			),
		).rejects.toThrow("1 to 32");
		await expect(
			call(
				"progress.report",
				{ summary: "x", evidence: [{ kind: "f", ref: "a" }], metrics: { n: "1" } },
				workerLane,
			),
		).rejects.toThrow("finite number");
		await expect(call("progress.assess", { task_id: running, verbose: true })).rejects.toThrow(
			"Unknown payload field: verbose",
		);
		await expect(call("progress.reassess", { task_id: running, claim: "done" })).rejects.toThrow("claim must be");
		await expect(call("progress.reassess", { task_id: running, verifier: "identity@1" })).rejects.toThrow(
			"require claim",
		);
		await expect(call("progress.reassess", { task_id: running, approve: true })).rejects.toThrow(
			"Unknown payload field: approve",
		);
		await expect(call("progress.assess", { task_id: "ultron-task-missing" })).rejects.toThrow("Unknown Ultron task");
		await expect(call("progress.nope", {})).rejects.toThrow("Unknown progress request");

		// Receipts per task are bounded.
		const bounded = new NativeRlmHost({ lane: async () => waitingLane() } as never, waitingLane() as never, {
			store: memoryStore(),
			modules: [createProgressModule({ store: createMemoryModuleStore(), maxReceiptsPerTask: 2 })],
		});
		hosts.push(bounded);
		const task = (await bounded.handle("agents.spawn", { definition: "identity@1", input: 1 }, context)) as {
			id: string;
		};
		const report = () =>
			bounded.handle(
				"progress.report",
				{ task_id: task.id, summary: "s", evidence: [{ kind: "k", ref: "r" }] },
				context,
			);
		await report();
		await report();
		await expect(report()).rejects.toThrow("maximum of 2 progress receipts");
	});

	test("always denies budget extension and records the reason", async () => {
		const { call, running, workerLane } = await setup();
		const decision = await call<Decision>(
			"progress.reassess",
			{ task_id: running, extend_budget: { tasks: 10 } },
			workerLane,
		);
		expect(decision.requester_task_id).toBe(running);
		expect(decision.budget).toMatchObject({
			decision: "denied",
			reason: expect.stringContaining("fixed at admission"),
		});
		expect(decision.assessment.classification).toBe("stalled");
		const root = await call<Decision>("progress.reassess", { task_id: running, extend_budget: true });
		expect(root.budget?.decision).toBe("denied");
		const history = await call<{ decisions: Decision[] }>("progress.history", { task_id: running });
		expect(history.decisions.map((item) => item.budget?.decision)).toEqual(["denied", "denied"]);
	});

	test("never verifies a completion claim without a passing verifier", async () => {
		const { call, running, workerLane } = await setup();
		const register = {
			id: "failing-check",
			version: "1",
			strategy: "deterministic",
			instructions: "Always fails.",
			inputSchema: {},
			outputSchema: {},
			maxRepairs: 0,
			inputDescription: "any",
			outputDescription: "any",
		};
		await call("agents.register", { definition: register });
		const done = await call<{ id: string }>(
			"agents.spawn",
			{ definition: "identity@1", input: { answer: 42 } },
			workerLane,
		);
		await call("agents.result", { id: done.id });
		const claim = (extra: Record<string, unknown>) =>
			call<Decision>("progress.reassess", { task_id: done.id, claim: "complete", ...extra }, workerLane);

		const none = await claim({});
		expect(none.claim).toMatchObject({ decision: "unverified", reason: expect.stringContaining("No verifier") });

		const unknown = await claim({ verifier: "no-such-check@1" });
		expect(unknown.claim).toMatchObject({
			decision: "unverified",
			reason: expect.stringContaining("could not be started"),
		});

		const failing = await claim({ verifier: "failing-check@1" });
		expect(failing.claim).toMatchObject({ decision: "unverified", verifier_status: "failed" });
		expect(failing.claim?.reason).toContain("check exploded");

		const notPassed = await claim({ verifier: "identity@1", verifier_input: { passed: false } });
		expect(notPassed.claim).toMatchObject({ decision: "unverified", verifier_status: "succeeded" });

		const nonBoolean = await claim({ verifier: "identity@1", verifier_input: { passed: "yes" } });
		expect(nonBoolean.claim).toMatchObject({ decision: "unverified", reason: expect.stringContaining("boolean") });

		// Default verifier input carries the task result; identity echoes it without a passed field.
		const defaultInput = await claim({ verifier: "identity@1" });
		expect(defaultInput.claim?.decision).toBe("unverified");

		const verified = await claim({ verifier: "identity@1", verifier_input: { passed: true } });
		expect(verified.claim).toMatchObject({ decision: "verified", verifier_status: "succeeded" });
		const tasks = await call<{ tasks: Array<{ id: string; parentId?: string }> }>("agents.tasks", {});
		expect(tasks.tasks.find((task) => task.id === verified.claim?.verifier_task_id)?.parentId).toBe(running);

		const onRunning = await call<Decision>("progress.reassess", {
			task_id: running,
			claim: "complete",
			verifier: "identity@1",
			verifier_input: { passed: true },
		});
		expect(onRunning.claim).toMatchObject({ decision: "rejected", reason: expect.stringContaining("running") });

		const history = await call<{ decisions: Decision[] }>("progress.history", { task_id: done.id }, workerLane);
		expect(history.decisions.map((item) => item.claim?.decision)).toEqual([
			"unverified",
			"unverified",
			"unverified",
			"unverified",
			"unverified",
			"unverified",
			"verified",
		]);
	});

	test("persists receipts and decisions across module instances and settles interrupted verifications", async () => {
		const progressStore = createMemoryModuleStore();
		const first = await setup(progressStore);
		await first.call(
			"progress.report",
			{ summary: "step", evidence: [{ kind: "test", ref: "suite-a" }], metrics: { passed: 3 } },
			first.workerLane,
		);
		await first.call("progress.reassess", { task_id: first.running, extend_budget: true });
		const saved = (await progressStore.read()) as { decisions: Array<Record<string, unknown>> };
		// Simulate an owner that died while a verifier was running.
		saved.decisions.push({
			id: "progress-decision-crashed",
			task_id: first.running,
			requester_task_id: null,
			at: 1,
			assessment: { classification: "busy", reasons: [], receipts: [] },
			claim: { claim: "complete", decision: "pending", reason: "Verifier running", verifier: "identity@1" },
		});
		await progressStore.write(saved as never);

		const reopened = createProgressModule({ store: progressStore, now: () => 0 });
		const api = {
			callerTaskId: () => null,
			tasks: async () => [
				{
					id: first.running,
					key: "k",
					fingerprint: "f".repeat(64),
					definition: "background-job@1",
					state: "running",
				},
			],
		} as never;
		await reopened.start?.(api);
		const history = (await reopened.handle(
			{ type: "progress.history", payload: { task_id: first.running }, caller: { lane: "main" }, context },
			api,
		)) as { receipts: Array<{ metrics?: unknown }>; decisions: Decision[] };
		expect(history.receipts).toHaveLength(1);
		expect(history.receipts[0].metrics).toEqual({ passed: 3 });
		expect(history.decisions.map((item) => item.budget?.decision ?? item.claim?.decision)).toEqual([
			"denied",
			"unverified",
		]);
		expect(history.decisions[1].claim?.reason).toContain("Owner ended");
	});

	test("rejects a corrupt stored document", async () => {
		const module = createProgressModule({ store: createMemoryModuleStore({ version: 1 }) });
		await expect(Promise.resolve(module.start?.({} as never))).rejects.toThrow("Invalid progress document");
	});

	test("a document in a newer format fails naming its namespace and version", async () => {
		const module = createProgressModule({ store: createMemoryModuleStore({ version: 2 }) });
		await expect(Promise.resolve(module.start?.({} as never))).rejects.toThrow(
			"ultron.module/progress has format version 2",
		);
	});
});
