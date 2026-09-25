import type { Context, JsonValue } from "@earendil-works/chord";
import { describe, expect, test } from "vitest";
import { NativeJevClient } from "../src/ultron/jev.ts";
import { MemoryError, type MemoryScope } from "../src/ultron/memory.ts";
import { createWorkerServices } from "../src/ultron/worker-services.ts";
import { durableStore, FakeHindsight, type GateLog, gate, open, scopes } from "./ultron-fake-hindsight.ts";

const context = {} as Context;

function session() {
	const values = new Map<string, { address: { namespace: string; key: string }; value: JsonValue }>();
	return {
		getValue: async (address: { namespace: string; key: string }) =>
			values.get(`${address.namespace}\0${address.key}`),
		setValue: async (address: { namespace: string; key: string }, value: JsonValue) => {
			values.set(`${address.namespace}\0${address.key}`, { address, value: structuredClone(value) });
		},
		scanValues: async () => [],
	};
}

/** A Jev System One endpoint that scores memory as not useful (below the 0.65 recall threshold). */
function lowRecallJev(requests: string[]) {
	return new NativeJevClient({
		apiKey: "test-key",
		baseUrl: "http://jev.test",
		fetch: async (_input, init) => {
			requests.push(String(init?.body));
			return new Response(JSON.stringify({ answers: { retrieve: { noul: 0.2 } } }), { status: 200 });
		},
	});
}

describe("A16 memory skip causes no retrieval; scopes survive consolidation", () => {
	test("a Jev skip decision sends no request of any kind to Hindsight, for every scope and entry point", async () => {
		const hindsight = new FakeHindsight();
		const log: GateLog = [];
		const memory = open(hindsight, durableStore(), gate(false, log));
		for (const scope of ["session", "project", "global"] as MemoryScope[]) {
			const prepared = await memory.prepare({ query: "what is the launcher path", scope, taskId: `task-${scope}` });
			expect(prepared).toMatchObject({ results: [], context: "" });
			expect(prepared.operation).toMatchObject({ state: "skipped", phase: "gate", references: [] });
			expect(prepared.operation.tags).toBeUndefined();
		}
		expect(log.map((request) => request.action)).toEqual(["recall", "recall", "recall"]);
		// API capture: zero recall calls, and no other Hindsight endpoint was touched either.
		expect(hindsight.captured).toEqual([]);

		// The production binding: Jev's real memoryGate scores low, so the worker service never reaches Hindsight.
		const jevRequests: string[] = [];
		const worker = createWorkerServices({
			session: session() as never,
			sessionId: "s1",
			cwd: "/work/app",
			jev: lowRecallJev(jevRequests),
			backend: hindsight.backend(),
		});
		const viaWorker = (await worker.handle(
			"memory.prepare",
			{ query: "explain closures", taskId: "task-worker" },
			context,
		)) as { operation: { state: string; gate: unknown }; results: unknown[] };
		expect(viaWorker.operation).toMatchObject({ state: "skipped", gate: { retrieve: false, probability: 0.2 } });
		const viaJevRecall = (await worker.handle("jev.recall", { prompt: "explain closures" }, context)) as {
			results: unknown[];
		};
		expect(viaJevRecall.results).toEqual([]);
		expect(jevRequests).toHaveLength(2);
		expect(hindsight.captured).toEqual([]);

		// No Jev configured is also a skip, never a retrieval.
		const noJev = createWorkerServices({
			session: session() as never,
			sessionId: "s1",
			cwd: "/work/app",
			backend: hindsight.backend(),
		});
		await expect(
			noJev.handle("memory.prepare", { query: "anything", taskId: "task-nojev" }, context),
		).resolves.toMatchObject({ operation: { state: "skipped" } });
		expect(hindsight.captured).toEqual([]);
	});

	test("scope tags are pinned through retain, correction and Hindsight consolidation; a cross-scope merge is refused", async () => {
		const hindsight = new FakeHindsight();
		const memory = open(hindsight, durableStore());
		const sessionFact = await memory.propose({ text: "session: use pnpm", evidence: [{ ref: "task:1" }] });
		const projectFact = await memory.propose({
			text: "project: tests live in test/",
			evidence: [{ ref: "task:2" }],
			scope: "project",
		});
		// Accepted is not stored: the correction waits until the receipt confirms retention.
		await expect(memory.get(sessionFact.memoryId!)).resolves.toMatchObject({ state: "stored" });
		const corrected = await memory.correct(sessionFact.memoryId!, {
			text: "session: use npm, not pnpm",
			evidence: [{ ref: "user:3" }],
		});
		for (const operation of [sessionFact, projectFact, corrected]) expect(operation.state).toBe("accepted");

		// Wire capture: every write, including the correction (the service's only merge/replace path),
		// carries exactly its scope's tags and pins Hindsight consolidation to those tags alone.
		const retains = hindsight.retains().map((call) => (call.body!.items as Record<string, unknown>[])[0]);
		expect(retains.map((item) => [item.document_id, item.tags, item.observation_scopes, item.update_mode])).toEqual([
			[sessionFact.memoryId, scopes.session, [scopes.session], "replace"],
			[projectFact.memoryId, scopes.project, [scopes.project], "replace"],
			[sessionFact.memoryId, scopes.session, [scopes.session], "replace"],
		]);

		hindsight.consolidate();
		const sessionRecall = await memory.prepare({ query: "package manager", taskId: "task-a" });
		const projectRecall = await memory.prepare({ query: "package manager", scope: "project", taskId: "task-b" });
		const sessionObservation = sessionRecall.results.find((item) => item.type === "observation")!;
		const projectObservation = projectRecall.results.find((item) => item.type === "observation")!;
		expect(sessionObservation).toMatchObject({ tags: scopes.session });
		expect(sessionObservation.text).toContain("use npm, not pnpm");
		expect(sessionObservation.text).not.toContain("tests live in test/");
		expect(projectObservation).toMatchObject({ tags: scopes.project });
		expect(projectObservation.text).not.toContain("npm");
		expect(hindsight.recalls().map((call) => call.body!.tags)).toEqual([scopes.session, scopes.project]);
		expect(hindsight.recalls().every((call) => call.body!.tags_match === "exact")).toBe(true);

		// Fault: a consolidation that ignores observation scopes produces a cross-scope observation.
		// Exact-tag recall would not return it from a correct backend; a faulty one is refused outright.
		hindsight.mergeAcrossScopes = true;
		hindsight.consolidate();
		expect(hindsight.observations.map((item) => item.tags)).toEqual([[...scopes.session, ...scopes.project]]);
		await expect(memory.prepare({ query: "package manager", taskId: "task-c" })).resolves.toMatchObject({
			results: [{ type: "world" }],
		});
		hindsight.leakObservations = true;
		const guarded = memory;
		await expect(guarded.prepare({ query: "package manager", taskId: "task-c", refresh: true })).rejects.toSatisfy(
			(error: unknown) => error instanceof MemoryError && error.code === "INVALID_RESPONSE",
		);
		const decisions = await guarded.why("task-c");
		expect(decisions.map((decision) => decision.state)).toEqual(["recalled", "failed"]);
		expect(decisions[1]).toMatchObject({ error: { code: "INVALID_RESPONSE" }, tags: scopes.session });
		expect(decisions[1].references).toBeUndefined();
	});
});
