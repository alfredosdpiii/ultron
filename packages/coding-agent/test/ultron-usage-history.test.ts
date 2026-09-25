import type { JsonValue } from "@earendil-works/chord";
import { afterEach, describe, expect, test } from "vitest";
import { inspectionDiscrepancies, reconcileRecords } from "../src/ultron/reconcile.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { NativeUsageLedger, type NativeUsageStore } from "../src/ultron/usage.ts";

/**
 * The usage document keeps the most recent roots call by call and folds older idle roots into a bounded
 * history summary, so a long session's document stays bounded while session totals stay exact.
 */

function clock(start = 1_000_000) {
	let now = start;
	return {
		now: () => now,
		advance: (ms: number) => {
			now += ms;
		},
	};
}

function durable() {
	let document: JsonValue | undefined;
	let generation = 0;
	return {
		get document() {
			return structuredClone(document) as JsonValue;
		},
		open(): NativeUsageStore {
			const mine = ++generation;
			return {
				read: async () => structuredClone(document),
				write: async (next) => {
					if (mine !== generation) throw new Error("owner fenced");
					document = structuredClone(next);
				},
			};
		},
	};
}

type Doc = { roots: Record<string, unknown>; history?: { roots: number; taskAdmissions: Record<string, number> } };

/** Runs `turns` root turns, each with one task admission and two priced model calls (binary fractions: exact sums). */
async function runTurns(ledger: NativeUsageLedger, time: ReturnType<typeof clock>, turns: number, from = 0) {
	for (let index = from; index < from + turns; index++) {
		const rootId = `turn:run-${index}`;
		const task = await ledger.reserve({ kind: "task", rootId, requestKey: `task-${index}` });
		for (const cost of [0.25, 0.125]) {
			const model = await ledger.reserve({ kind: "model", rootId, taskId: `task-${index}` });
			time.advance(3);
			await ledger.settle(model, {
				status: "succeeded",
				usage: { inputTokens: 100 + index, outputTokens: 7, totalTokens: 107 + index, cost },
			});
		}
		await ledger.settle(task, { status: "succeeded", usage: { wallMs: 10 } });
		time.advance(2_000);
	}
}

describe("usage history folding", () => {
	test("many turns keep the document bounded and session totals exact", async () => {
		const time = clock();
		const folding = durable();
		const reference = durable();
		const limits = { maxWallMs: 1_000 };
		const ledger = new NativeUsageLedger(folding.open(), { limits, now: time.now, detailedRoots: 5 });
		const mirror = clock();
		const unbounded = new NativeUsageLedger(reference.open(), { limits, now: mirror.now, detailedRoots: 10_000 });
		await runTurns(ledger, time, 200);
		await runTurns(unbounded, mirror, 200);

		const document = folding.document as unknown as Doc;
		expect(Object.keys(document.roots).length).toBeLessThanOrEqual(5);
		expect(document.history?.roots).toBe(195);
		expect(document.history?.taskAdmissions).toMatchObject({ succeeded: 195 });
		// Bounded: the folded document is a small fraction of the detailed one and does not grow with more turns.
		const size = JSON.stringify(document).length;
		expect(size).toBeLessThan(JSON.stringify(reference.document).length / 20);
		await runTurns(ledger, time, 200, 200);
		expect(JSON.stringify(folding.document).length).toBeLessThan(size * 1.2);

		await runTurns(unbounded, mirror, 200, 200);
		const folded = (await ledger.status("turn:run-399")).session;
		const detailed = (await unbounded.status("turn:run-399")).session;
		expect(folded.usage).toEqual(detailed.usage);
		expect(folded.spentUsd).toBe(detailed.spentUsd);
		expect(folded.spentUsd).toBe(400 * 0.375);
		expect(folded.usage).toMatchObject({ calls: 1200, taskCalls: 400, modelCalls: 800, unknownCalls: 400 });
		expect(folded).toMatchObject({ roots: 5, foldedRoots: 395 });
		// The shown root keeps its own detail.
		expect((await ledger.status("turn:run-399")).usage).toEqual((await unbounded.status("turn:run-399")).usage);
	});

	test("a root with an open reservation or an unexpired deadline is not folded", async () => {
		const time = clock();
		const ledger = new NativeUsageLedger(undefined, {
			limits: { maxWallMs: 1_000 },
			now: time.now,
			detailedRoots: 1,
		});
		const open = await ledger.reserve({ kind: "task", rootId: "job:open", requestKey: "open" });
		time.advance(5_000);
		await ledger.reserve({ kind: "task", rootId: "turn:fresh-1", requestKey: "f1" });
		time.advance(1);
		await ledger.reserve({ kind: "task", rootId: "turn:fresh-2", requestKey: "f2" });
		const status = await ledger.status("job:open");
		// job:open still holds its reservation; turn:fresh-1 is before its deadline; neither folds.
		expect(status.activeReservations).toBe(1);
		expect(status.session).toMatchObject({ roots: 3, foldedRoots: 0 });
		// Once settled and past every deadline, the older roots fold on the next write.
		await ledger.settle(open, { status: "succeeded" });
		time.advance(5_000);
		await ledger.reserve({ kind: "task", rootId: "turn:later", requestKey: "l" });
		const after = await ledger.status("turn:later");
		expect(after.session).toMatchObject({ roots: 3, foldedRoots: 1 });
		// The folded one is the oldest idle root; the two with open reservations stay detailed.
		expect((await ledger.status("turn:fresh-2")).activeReservations).toBe(1);
		expect(after.session.usage.taskCalls).toBe(1);
	});

	test("restart reads the history, keeps totals, and compacts an oversized document on reconcile", async () => {
		const time = clock();
		const state = durable();
		const first = new NativeUsageLedger(state.open(), {
			limits: { maxWallMs: 1_000 },
			now: time.now,
			detailedRoots: 50,
		});
		await runTurns(first, time, 30);
		const before = (await first.status()).session;
		expect(before.foldedRoots).toBe(0);

		// The next owner keeps fewer roots in detail; its startup reconcile folds the rest without losing totals.
		const second = new NativeUsageLedger(state.open(), {
			limits: { maxWallMs: 1_000 },
			now: time.now,
			detailedRoots: 4,
		});
		await second.reconcile([]);
		const after = (await second.status()).session;
		expect(after.usage).toEqual(before.usage);
		expect(after.spentUsd).toBe(before.spentUsd);
		expect(after).toMatchObject({ roots: 4, foldedRoots: 26 });
		// The old owner is fenced, and a third owner reads the folded document unchanged.
		await expect(first.reserve({ kind: "task", rootId: "turn:z", requestKey: "z" })).rejects.toThrow();
		const third = new NativeUsageLedger(state.open(), {
			limits: { maxWallMs: 1_000 },
			now: time.now,
			detailedRoots: 4,
		});
		expect((await third.status()).session).toEqual(after);
		await runTurns(third, time, 3, 30);
		expect((await third.status()).session.usage.calls).toBe(before.usage.calls + 9);
		// A corrupted history is refused rather than silently reset.
		const corrupt = structuredClone(state.document) as unknown as { history: { usage: { calls: number } } };
		corrupt.history.usage.calls = -1;
		const broken = new NativeUsageLedger({ read: async () => corrupt as never, write: async () => {} });
		await expect(broken.status()).rejects.toThrow("Invalid usage ledger");
	});
});

/** Lanes that complete one provider turn with reported usage. */
function meteredHarness() {
	const lanes = new Map<string, object>();
	return {
		lane: async (name: string) => {
			let lane = lanes.get(name);
			if (!lane) {
				let entries: unknown[] = [];
				lane = {
					getActiveTools: async () => [],
					setModel: async () => {},
					steer: async () => ({ ok: true, value: {} }),
					abort: async () => ({ ok: true }),
					findEntries: async () => entries,
					prompt: async (text: string) => {
						entries = [
							{
								id: "tip",
								type: "message",
								message: {
									role: "assistant",
									content: [{ type: "text", text: "done" }],
									usage: {
										input: 40,
										output: 8,
										cacheRead: 0,
										cacheWrite: 0,
										totalTokens: 48,
										cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.5 },
									},
								},
							},
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

const hosts: NativeRlmHost[] = [];
afterEach(async () => {
	for (const host of hosts.splice(0)) await host.close().catch(() => {});
});

function memoryJournal() {
	let document: JsonValue | undefined;
	return {
		get document() {
			return structuredClone(document);
		},
		open: () => ({
			read: async () => structuredClone(document),
			write: async (next: JsonValue) => {
				document = structuredClone(next);
			},
		}),
	};
}

describe("reconciliation across root turns", () => {
	test("agents.status of the current turn has no false discrepancies, before and after folding", async () => {
		const time = clock();
		const tasks = memoryJournal();
		const usage = durable();
		const host = new NativeRlmHost(meteredHarness() as never, {} as never, {
			store: tasks.open(),
			definitionStore: memoryJournal().open(),
			usage: new NativeUsageLedger(usage.open(), { limits: { maxWallMs: 1_000 }, now: time.now, detailedRoots: 3 }),
			rootTurns: true,
			now: time.now,
		});
		hosts.push(host);
		const call = <T = Record<string, unknown>>(type: string, payload: Record<string, unknown> = {}) =>
			host.handle(type, payload, {} as never) as Promise<T>;
		const check = async () => {
			const status = await call("agents.status");
			const reconciliation = reconcileRecords({ tasks: tasks.document, usage: usage.document });
			expect(reconciliation.discrepancies).toEqual([]);
			expect(inspectionDiscrepancies(reconciliation, status)).toEqual([]);
			return { status, reconciliation };
		};
		for (let turn = 0; turn < 8; turn++) {
			host.beginRootTurn(`run-${turn}`);
			const child = await call<{ id: string }>("agents.spawn", {
				definition: "rlm-child@1",
				input: { prompt: "w" },
			});
			expect(await call("agents.result", { id: child.id })).toMatchObject({ status: "succeeded" });
			const { status, reconciliation } = await check();
			// The status view is the current turn only; the session view carries every turn.
			expect((status as { usage: { usage: { modelCalls: number } } }).usage.usage.modelCalls).toBe(1);
			expect(reconciliation.sessionTotals.modelCalls).toBe(turn + 1);
			host.endRootTurn(`run-${turn}`);
			time.advance(2_000);
		}
		const { status, reconciliation } = await check();
		const session = (status as { usage: { session: { foldedRoots: number; spentUsd: number } } }).usage.session;
		expect(session.foldedRoots).toBeGreaterThan(0);
		expect(session.spentUsd).toBe(8 * 0.5);
		// Journal tasks of folded roots are accounted by the history's admission counts.
		expect(reconciliation.tasks.filter((task) => task.admission === "folded").length).toBe(session.foldedRoots);

		// Negative control: a forged session total is caught.
		const forged = structuredClone(status) as { usage: { session: { usage: { calls: number } } } };
		forged.usage.session.usage.calls += 1;
		expect(inspectionDiscrepancies(reconciliation, forged).join("\n")).toMatch(/inspected session usage calls/);
	});
});
