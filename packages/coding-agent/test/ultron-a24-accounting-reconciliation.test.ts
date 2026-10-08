import type { JsonValue } from "@ultron/chord";
import { afterEach, describe, expect, test } from "vitest";
import { createProgressModule } from "../src/ultron/progress.ts";
import { inspectionDiscrepancies, reconcileRecords } from "../src/ultron/reconcile.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { createScheduleModule } from "../src/ultron/schedules.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

const context = {} as never;

/** One durable document with owner fencing: opening a new owner makes the previous owner's writes fail. */
function durable() {
	let document: JsonValue | undefined;
	let generation = 0;
	return {
		get document() {
			return structuredClone(document);
		},
		set document(next: JsonValue | undefined) {
			document = structuredClone(next);
		},
		open() {
			const mine = ++generation;
			return {
				read: async () => structuredClone(document),
				write: async (next: JsonValue) => {
					if (mine !== generation) throw new Error("owner fenced");
					document = structuredClone(next);
				},
			};
		},
	};
}

function assistant(id: string, text: string, usage?: { input: number; output: number; cost: number }) {
	return {
		id,
		type: "message",
		message: {
			role: "assistant",
			content: [{ type: "text", text }],
			...(usage === undefined
				? {}
				: {
						usage: {
							input: usage.input,
							output: usage.output,
							cacheRead: 0,
							cacheWrite: 0,
							totalTokens: usage.input + usage.output,
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: usage.cost },
						},
					}),
		},
	};
}

/**
 * Lanes whose behavior follows the child prompt: "work" completes two provider turns with reported
 * usage, "unmetered" completes without usage, "fail" fails the model call, "hang" waits for abort.
 */
function fakeHarness() {
	const lanes = new Map<string, object>();
	return {
		lane: async (name: string) => {
			let lane = lanes.get(name);
			if (!lane) {
				let entries: unknown[] = [];
				let release: ((value: unknown) => void) | undefined;
				lane = {
					getActiveTools: async () => [],
					setModel: async () => {},
					steer: async () => ({ ok: true, value: {} }),
					abort: async () => {
						release?.({ ok: false, error: { kind: "aborted" } });
						return { ok: true };
					},
					findEntries: async () => entries,
					prompt: async (text: string) => {
						if (text === "fail") return { ok: false, error: { kind: "provider", message: "503" } };
						if (text === "hang")
							return new Promise((resolve) => {
								release = resolve;
							});
						entries =
							text === "unmetered"
								? [assistant("tip", "done"), { id: "from", type: "message", message: { role: "user" } }]
								: [
										assistant("tip", "done", { input: 100, output: 20, cost: 0.01 }),
										assistant("mid", "tool call", { input: 50, output: 10, cost: 0.005 }),
										{ id: "from", type: "message", message: { role: "user", content: text } },
									];
						return { ok: true, value: { status: "completed", tipId: "tip", fromTipId: "from" } };
					},
				};
				lanes.set(name, lane);
			}
			return lane;
		},
	};
}

type Stores = ReturnType<typeof stores>;
function stores() {
	return { tasks: durable(), usage: durable(), progress: durable(), schedules: durable(), definitions: durable() };
}

const hosts: NativeRlmHost[] = [];
afterEach(async () => {
	for (const host of hosts.splice(0)) await host.close().catch(() => {});
});

function build(state: Stores) {
	const host = new NativeRlmHost(fakeHarness() as never, {} as never, {
		store: state.tasks.open(),
		definitionStore: state.definitions.open(),
		usage: new NativeUsageLedger(state.usage.open(), { limits: { maxAdmittedTasks: 24 } }),
		deterministic: async ({ definition, input }) => {
			if (definition.id === "check-pass") return { passed: true };
			if (definition.id === "check-fail") return { passed: false };
			return input;
		},
		modules: [
			createProgressModule({ store: state.progress.open() }),
			createScheduleModule({ store: state.schedules.open(), tickIntervalMs: 0 }),
		],
	});
	hosts.push(host);
	return <T = Record<string, unknown>>(type: string, payload: Record<string, unknown> = {}) =>
		host.handle(type, payload, context) as Promise<T>;
}

function check(id: string) {
	return {
		id,
		version: "1",
		strategy: "deterministic",
		instructions: "Report whether the check passed.",
		inputSchema: {},
		outputSchema: {
			type: "object",
			properties: { passed: { type: "boolean" } },
			required: ["passed"],
		},
		maxRepairs: 0,
		inputDescription: "Any JSON value",
		outputDescription: "{passed:boolean}",
	};
}

async function settle(call: ReturnType<typeof build>, id: string) {
	return call("agents.result", { id });
}

function records(state: Stores) {
	return reconcileRecords({
		tasks: state.tasks.document,
		usage: state.usage.document,
		progress: state.progress.document,
		schedules: state.schedules.document,
	});
}

describe("A24 costs, task transitions, and inspection agree on one record", () => {
	test("reconstructs a mixed run from the journal, ledger, progress and schedule records and matches agents.status", async () => {
		const state = stores();
		const call = build(state);
		await call("agents.register", { definition: check("check-pass") });
		await call("agents.register", { definition: check("check-fail") });

		const direct = await call("agents.invoke", { definition: "identity@1", input: { n: 1 }, key: "direct" });
		expect(direct).toMatchObject({ status: "succeeded" });
		// Idempotent lookup: no second admission, charge, or execution.
		expect(await call("agents.invoke", { definition: "identity@1", input: { n: 1 }, key: "direct" })).toEqual(direct);
		await expect(call("agents.invoke", { definition: "identity@1", input: { n: 2 }, key: "direct" })).rejects.toThrow(
			"Idempotency key reused",
		);

		const metered = await call<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "work" },
		});
		expect(await settle(call, metered.id)).toMatchObject({ status: "succeeded", value: "done" });
		const unmetered = await call<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "unmetered" },
		});
		expect(await settle(call, unmetered.id)).toMatchObject({ status: "succeeded" });
		const failed = await call<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "fail" },
		});
		expect(await settle(call, failed.id)).toMatchObject({ status: "failed" });
		const hanging = await call<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "hang" },
		});
		await new Promise((resolve) => setTimeout(resolve, 5));
		expect(await call("agents.cancel", { id: hanging.id })).toEqual({ cancelled: true });

		await call("progress.report", {
			task_id: metered.id,
			summary: "computed answer",
			evidence: [{ kind: "file", ref: "out.txt" }],
		});
		const decision = await call<{ claim: { decision: string; verifier_task_id: string } }>("progress.reassess", {
			task_id: metered.id,
			claim: "complete",
			verifier: "check-pass@1",
		});
		expect(decision.claim.decision).toBe("verified");
		const goal = await call<{ id: string }>("goals.create", {
			title: "release",
			required_checks: ["check-pass@1", "check-fail@1"],
		});
		const verified = await call<{ verification: { status: string } }>("goals.verify", { id: goal.id });
		expect(verified.verification.status).toBe("unachieved");
		await call("schedules.create", { definition: "identity@1", input: { tick: true }, every_ms: 60_000 });
		const tick = await call<{ fired: unknown[] }>("schedules.tick");
		expect(tick.fired).toHaveLength(1);

		const status = await call("agents.status");
		const reconciliation = records(state);
		expect(reconciliation.discrepancies).toEqual([]);
		expect(inspectionDiscrepancies(reconciliation, status)).toEqual([]);

		// Per-task accounting: every journal task has exactly one admission settlement matching its state.
		const byId = new Map(reconciliation.tasks.map((task) => [task.id, task]));
		expect(reconciliation.tasks).toHaveLength(9); // direct, 4 children, verifier, 2 goal checks, schedule firing
		expect(byId.get(metered.id)).toMatchObject({
			state: "completed",
			admission: "succeeded",
			usage: { taskCalls: 1, modelCalls: 1, unknownCalls: 1 },
			modelUsage: { modelCalls: 1, inputTokens: 150, outputTokens: 30, totalTokens: 180, unknownCalls: 0 },
		});
		expect(byId.get(metered.id)!.modelUsage.cost).toBeCloseTo(0.015, 12);
		// An admission is not a provider call and a run without reported usage stays unknown, never zero.
		expect(byId.get(metered.id)!.usage.inputTokens).toBeNull();
		expect(byId.get(unmetered.id)!.modelUsage).toMatchObject({ modelCalls: 1, cost: null, unknownCalls: 1 });
		expect(byId.get(failed.id)).toMatchObject({ state: "failed", admission: "failed", usage: { modelCalls: 1 } });
		expect(byId.get(hanging.id)).toMatchObject({ state: "cancelled", admission: "cancelled" });
		expect(byId.get(decision.claim.verifier_task_id)).toMatchObject({ resultStatus: "succeeded" });

		// Sum of per-task usage plus unattributed calls equals the ledger's own totals.
		const ledger = (status as { usage: { usage: Record<string, number | null> } }).usage.usage;
		const sum = (field: "calls" | "taskCalls" | "modelCalls" | "wallMs") =>
			reconciliation.tasks.reduce((total, task) => total + task.usage[field], 0) +
			reconciliation.unattributed[field];
		expect(sum("calls")).toBe(ledger.calls);
		expect(sum("taskCalls")).toBe(ledger.taskCalls);
		expect(sum("modelCalls")).toBe(ledger.modelCalls);
		expect(sum("wallMs")).toBeCloseTo(ledger.wallMs as number, 6);
		expect(reconciliation.unattributed).toMatchObject({ calls: 0 });
		expect(ledger).toMatchObject({ taskCalls: 9, modelCalls: 4, calls: 13, cost: null });

		// Timeline: every task is admitted before it settles, and receipts/decisions/verification appear.
		for (const task of reconciliation.tasks) {
			const events = reconciliation.timeline.filter((event) => event.taskId === task.id);
			const admitted = events.find((event) => event.kind === "task.admitted");
			const settled = events.find((event) => event.kind === "task.settled");
			expect(admitted, task.id).toBeDefined();
			expect(settled!.at).toBeGreaterThanOrEqual(admitted!.at);
		}
		const kinds = reconciliation.timeline.map((event) => event.kind);
		for (const kind of ["model.call", "progress.receipt", "progress.decision", "goal.verification", "schedule.fired"])
			expect(kinds).toContain(kind);
		expect(reconciliation.timeline.find((event) => event.kind === "progress.decision")?.detail).toBe(
			"finished/verified",
		);
		const times = reconciliation.timeline.map((event) => event.at);
		expect(times).toEqual([...times].sort((left, right) => left - right));
	});

	test("a crashed owner's open work reconciles as interrupted and unknown after restart, and tampering is detected", async () => {
		const state = stores();
		const first = build(state);
		const done = await first<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "work" },
		});
		await settle(first, done.id);
		const open = await first<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "hang" },
		});
		await new Promise((resolve) => setTimeout(resolve, 5));
		// Before the crash the ledger holds active task and model reservations for the open task.
		expect(records(state).activeReservations).toBe(2);

		// Crash: a new owner opens the same records; the old owner's writes are fenced.
		const restarted = build(state);
		const status = await restarted("agents.status");
		const reconciliation = records(state);
		expect(reconciliation.discrepancies).toEqual([]);
		expect(inspectionDiscrepancies(reconciliation, status)).toEqual([]);
		expect(reconciliation.activeReservations).toBe(0);
		expect(reconciliation.tasks.find((task) => task.id === open.id)).toMatchObject({
			state: "interrupted",
			resultStatus: "interrupted",
			admission: "unknown",
			usage: { taskCalls: 1, modelCalls: 1, unknownCalls: 2, cost: null },
		});
		expect(reconciliation.tasks.find((task) => task.id === done.id)).toMatchObject({
			admission: "succeeded",
			modelUsage: { inputTokens: 150, outputTokens: 30 },
		});

		// Negative controls: the checker is not vacuous.
		const usage = state.usage.document as { roots: Record<string, { calls: Array<{ kind: string }> }> };
		const root = Object.values(usage.roots)[0]!;
		const dropped = root.calls.findIndex((item) => item.kind === "task");
		root.calls.splice(dropped, 1);
		expect(reconcileRecords({ tasks: state.tasks.document, usage }).discrepancies.join("\n")).toMatch(
			/admission is missing/,
		);
		const forged = structuredClone(status) as {
			tasks: Array<{ state: string }>;
			usage: { usage: { calls: number } };
		};
		forged.tasks[0]!.state = "failed";
		forged.usage.usage.calls += 1;
		const problems = inspectionDiscrepancies(reconciliation, forged);
		expect(problems.some((problem) => problem.includes("inspected failed"))).toBe(true);
		expect(problems.some((problem) => problem.startsWith("inspected usage calls"))).toBe(true);
	});
});
