import type { JsonValue } from "@earendil-works/chord";
import { describe, expect, test } from "vitest";
import { createFamilyModule } from "../src/ultron/family.ts";
import { createProgressModule } from "../src/ultron/progress.ts";
import { createMemoryModuleStore } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;

function memoryStore(): NativeHostStore & { value: JsonValue | undefined } {
	return {
		value: undefined,
		async read() {
			return structuredClone(this.value);
		},
		async write(next) {
			this.value = structuredClone(next);
		},
	};
}

type LaneProbe = {
	started: Promise<void>;
	steered: string[];
	aborts: number;
	finish(text: string): void;
};

/** Lanes that block in prompt until the test finishes them or the host aborts them. */
function controlledHarness() {
	const probes = new Map<string, LaneProbe>();
	const lanes = new Map<string, object>();
	const probe = (name: string): LaneProbe => {
		let existing = probes.get(name);
		if (!existing) {
			let markStarted!: () => void;
			const started = new Promise<void>((resolve) => {
				markStarted = resolve;
			});
			let release: ((value: unknown) => void) | undefined;
			let text = "";
			const created: LaneProbe = {
				started,
				steered: [],
				aborts: 0,
				finish(value) {
					text = value;
					release?.({ ok: true, value: { status: "completed", tipId: "tip", fromTipId: null } });
				},
			};
			existing = created;
			probes.set(name, created);
			lanes.set(name, {
				getActiveTools: async () => [],
				setModel: async () => {},
				prompt: () =>
					new Promise((resolve) => {
						release = resolve;
						markStarted();
					}),
				steer: async (message: string) => {
					created.steered.push(message);
					return { ok: true, value: {} };
				},
				abort: async () => {
					created.aborts += 1;
					release?.({ ok: false, error: { kind: "aborted" } });
					return { ok: true };
				},
				findEntries: async () => [
					{ id: "tip", type: "message", message: { role: "assistant", content: [{ type: "text", text }] } },
				],
			});
		}
		return existing;
	};
	return {
		harness: {
			lane: async (name: string) => {
				probe(name);
				return lanes.get(name);
			},
		},
		probe,
	};
}

const laneOf = (id: string) => `ultron.rlm-child.${id}`;

describe("A42 human child intervention and subtree stop preserve unrelated work", () => {
	test("inspect, correct, and stop one child's subtree while the sibling continues and completes", async () => {
		const { harness, probe } = controlledHarness();
		const store = memoryStore();
		const host = new NativeRlmHost(harness as never, {} as never, {
			store,
			modules: [
				createFamilyModule({ store: createMemoryModuleStore() }),
				createProgressModule({ store: createMemoryModuleStore() }),
			],
		});
		const call = <T = Record<string, unknown>>(type: string, payload: Record<string, unknown>, lane?: string) =>
			host.handle(type, payload, context, lane === undefined ? undefined : { lane }) as Promise<T>;
		const spawn = async (prompt: string, lane?: string) =>
			(await call<{ id: string }>("agents.spawn", { definition: "rlm-child@1", input: { prompt } }, lane)).id;

		// Two children of the root; child A delegates to a grandchild, which delegates further.
		const a = await spawn("child A: audit");
		const b = await spawn("child B: summarize");
		await probe(laneOf(a)).started;
		await probe(laneOf(b)).started;
		const grandchild = await spawn("grandchild of A", laneOf(a));
		await probe(laneOf(grandchild)).started;
		const greatGrandchild = await spawn("great-grandchild of A", laneOf(grandchild));
		await probe(laneOf(greatGrandchild)).started;

		// Both children report progress from their own lanes.
		await call(
			"progress.report",
			{ summary: "A scanned 3 files", evidence: [{ kind: "file", ref: "a.ts" }] },
			laneOf(a),
		);
		await call("progress.report", { summary: "B read input", evidence: [{ kind: "file", ref: "b.md" }] }, laneOf(b));

		// Inspect child A: bounded task state plus its progress history, with no model call.
		expect(await call("agents.inspect", { id: a })).toEqual({ id: a, definition: "rlm-child@1", state: "running" });
		const history = await call<{ receipts: Array<{ summary: string; reporter_task_id: string }> }>(
			"progress.history",
			{ task_id: a },
		);
		expect(history.receipts).toMatchObject([{ summary: "A scanned 3 files", reporter_task_id: a }]);
		expect((await call("agents.status", {})) as { tasks: unknown[] }).toMatchObject({
			tasks: [
				{ id: a, state: "running" },
				{ id: b, state: "running" },
				{ id: grandchild, parentId: a, state: "running" },
				{ id: greatGrandchild, parentId: grandchild, state: "running" },
			],
		});

		// Human correction: a steering message routed to child A only, delivered into its live lane.
		const sent = await call<{ id: string; state: string; steered: boolean }>("agent_message.send", {
			message: "Stop auditing tests; focus on src/",
			receiver_role: "child",
			receiver_id: a,
			steer: true,
		});
		expect(sent).toMatchObject({ state: "delivered", steered: true });
		expect(probe(laneOf(a)).steered).toHaveLength(1);
		expect(probe(laneOf(a)).steered[0]).toContain("Verified sender: the root agent");
		expect(probe(laneOf(a)).steered[0]).toContain(JSON.stringify("Stop auditing tests; focus on src/"));
		expect(probe(laneOf(b)).steered).toEqual([]);

		// Stop child A's subtree.
		expect(await call("agents.cancel", { id: a })).toEqual({ cancelled: true });
		for (const id of [a, grandchild, greatGrandchild])
			expect(await call("agents.result", { id })).toMatchObject({ status: "cancelled", verification: "unverified" });
		await new Promise((resolve) => setTimeout(resolve, 0));
		for (const id of [a, grandchild, greatGrandchild]) expect(probe(laneOf(id)).aborts).toBe(1);
		expect(probe(laneOf(b)).aborts).toBe(0);
		expect(await call("agents.inspect", { id: b })).toMatchObject({ state: "running" });

		// A stopped child can no longer receive corrections; the sibling can.
		await expect(
			call("agent_message.send", { message: "late", receiver_role: "child", receiver_id: a, steer: true }),
		).rejects.toThrow("already finished");
		await call("agent_message.send", { message: "keep going", receiver_role: "child", receiver_id: b });

		// The unrelated child keeps working, reports new evidence, and completes.
		await call(
			"progress.report",
			{ summary: "B drafted", evidence: [{ kind: "file", ref: "summary.md" }] },
			laneOf(b),
		);
		const inbox = await call<{ messages: Array<{ content: string }> }>("agent_message.receive", {}, laneOf(b));
		expect(inbox.messages.map((message) => message.content)).toEqual(["keep going"]);
		probe(laneOf(b)).finish("B summary");
		expect(await call("agents.result", { id: b })).toMatchObject({ status: "succeeded", value: "B summary" });

		// Evidence readback from durable records shows both outcomes.
		const status = (await call("agents.status", {})) as {
			tasks: Array<{ id: string; state: string; result?: { status: string; error?: string } }>;
		};
		const byId = new Map(status.tasks.map((task) => [task.id, task]));
		expect(byId.get(a)).toMatchObject({ state: "cancelled", result: { error: "Ultron task cancelled" } });
		expect(byId.get(grandchild)).toMatchObject({
			state: "cancelled",
			result: { error: `Ancestor ${a} cancelled: Ultron task cancelled` },
		});
		expect(byId.get(greatGrandchild)).toMatchObject({ state: "cancelled" });
		expect(byId.get(b)).toMatchObject({ state: "completed", result: { status: "succeeded" } });
		const journal = store.value as { tasks: Array<{ id: string; state: string }> };
		expect(Object.fromEntries(journal.tasks.map((task) => [task.id, task.state]))).toEqual({
			[a]: "cancelled",
			[b]: "completed",
			[grandchild]: "cancelled",
			[greatGrandchild]: "cancelled",
		});
		const messages = await call<{ sent: Array<{ receiver_id: string; state: string; delivered_via?: string }> }>(
			"agent_message.list",
			{},
		);
		expect(messages.sent).toMatchObject([
			{ receiver_id: a, state: "delivered", delivered_via: "steer" },
			{ receiver_id: b, state: "delivered", delivered_via: "receive" },
		]);
		const bHistory = await call<{ receipts: Array<{ summary: string }> }>("progress.history", { task_id: b });
		expect(bHistory.receipts.map((receipt) => receipt.summary)).toEqual(["B read input", "B drafted"]);
		expect(await call("progress.assess", { task_id: a })).toMatchObject({
			classification: "finished",
			result_status: "cancelled",
		});
		await host.close();
	});

	test("stopping a finished child still stops its live descendants and leaves siblings alone", async () => {
		const { harness, probe } = controlledHarness();
		const host = new NativeRlmHost(harness as never, {} as never, { store: memoryStore() });
		const call = <T = Record<string, unknown>>(type: string, payload: Record<string, unknown>, lane?: string) =>
			host.handle(type, payload, context, lane === undefined ? undefined : { lane }) as Promise<T>;
		const spawn = async (prompt: string, lane?: string) =>
			(await call<{ id: string }>("agents.spawn", { definition: "rlm-child@1", input: { prompt } }, lane)).id;
		const parent = await spawn("parent");
		const sibling = await spawn("sibling");
		await probe(laneOf(parent)).started;
		const orphanable = await spawn("background grandchild", laneOf(parent));
		await probe(laneOf(orphanable)).started;
		probe(laneOf(parent)).finish("parent done");
		expect(await call("agents.result", { id: parent })).toMatchObject({ status: "succeeded" });
		expect(await call("agents.inspect", { id: orphanable })).toMatchObject({ state: "running" });

		// The parent's own terminal result is not rewritten, but its live subtree is stopped.
		expect(await call("agents.cancel", { id: parent })).toEqual({ cancelled: false });
		expect(await call("agents.result", { id: parent })).toMatchObject({ status: "succeeded" });
		expect(await call("agents.result", { id: orphanable })).toMatchObject({ status: "cancelled" });
		expect(await call("agents.inspect", { id: sibling })).toMatchObject({ state: "running" });
		probe(laneOf(sibling)).finish("sibling done");
		expect(await call("agents.result", { id: sibling })).toMatchObject({
			status: "succeeded",
			value: "sibling done",
		});
		await host.close();
	});
});
