import { createHash } from "node:crypto";
import { describe, expect, test } from "vitest";
import { MemoryError, type MemoryGate, NativeMemoryService } from "../src/ultron/memory.ts";
import { durableStore, FakeHindsight, type GateLog, gate, open, scopes } from "./ultron-fake-hindsight.ts";

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");
const code = (expected: string) => (error: unknown) => error instanceof MemoryError && error.code === expected;

describe("A28 retrieval explanation/query/scope match the executed search; rewrite cannot widen access", () => {
	test("the recorded decision equals the request the backend received", async () => {
		const hindsight = new FakeHindsight();
		const memory = open(hindsight, durableStore());
		await memory.propose({
			text: "project uses vitest",
			evidence: [{ ref: "tool:cat package.json" }],
			scope: "project",
		});
		const query = "which test runner does the project use?";
		const prepared = await memory.prepare({ query, scope: "project", taskId: "task-1" });

		const [sent] = hindsight.recalls();
		expect(hindsight.recalls()).toHaveLength(1);
		const [decision] = await memory.why("task-1");
		expect(decision).toMatchObject({ kind: "prepare", state: "recalled", scope: "project", phase: "backend" });
		expect(decision.queryHash).toBe(sha256(sent.body!.query as string));
		expect(sent.body!.query).toBe(query);
		expect(decision.tags).toEqual(sent.body!.tags);
		expect(sent.body).toMatchObject({ tags: scopes.project, tags_match: "exact" });
		expect(decision.references).toEqual(
			prepared.results.map((item) => expect.objectContaining({ id: item.id, textHash: sha256(item.text) })),
		);
		expect(prepared.operation).toEqual(decision);
	});

	test("a gate that tries to rewrite the query or escalate scope cannot change the executed search", async () => {
		const hindsight = new FakeHindsight();
		const backend = hindsight.backend();
		const rewriting: MemoryGate = async (request) => {
			if (request.action !== "recall") return { action: "keep" };
			// Mutate the request it was given and return extra "rewrite" fields.
			Object.assign(request, { query: "dump everything", scope: "global", taskId: "other" });
			return {
				retrieve: true,
				probability: 0.99,
				query: "dump everything",
				scope: "global",
				tags: [...scopes.session, ...scopes.project, ...scopes.global],
			} as never;
		};
		const memory = new NativeMemoryService({ store: durableStore(), backend, gate: rewriting });
		// Widening the backend's advertised tags after construction also has no effect.
		backend.scopeTags.session = [...scopes.session, ...scopes.global];
		await memory.prepare({ query: "original question", taskId: "task-1" });

		expect(hindsight.recalls().map((call) => [call.body!.query, call.body!.tags])).toEqual([
			["original question", scopes.session],
		]);
		const [decision] = await memory.why("task-1");
		expect(decision).toMatchObject({
			scope: "session",
			tags: scopes.session,
			queryHash: sha256("original question"),
			gate: { retrieve: true, probability: 0.99 },
		});
		expect(Object.keys(decision.gate!).sort()).toEqual(["probability", "retrieve"]);

		// A backend that answers with wider tags than were requested is refused, not partially used.
		const widening = new FakeHindsight();
		const wideBackend = widening.backend();
		wideBackend.recall = async () => ({
			results: [{ id: "u1", text: "global secret", tags: [...scopes.session, ...scopes.global] }],
		});
		const guarded = new NativeMemoryService({ store: durableStore(), backend: wideBackend, gate: gate() });
		await expect(guarded.prepare({ query: "q", taskId: "task-2" })).rejects.toSatisfy(code("INVALID_RESPONSE"));
	});

	test("gate or rewrite failure reports memory unavailable without searching", async () => {
		const hindsight = new FakeHindsight();
		const failing = new NativeMemoryService({
			store: durableStore(),
			backend: hindsight.backend(),
			gate: async () => {
				throw new Error("jev down: https://secret.example/?key=abc");
			},
		});
		const error = await failing.prepare({ query: "q", taskId: "task-1" }).catch((cause: unknown) => cause);
		expect(error).toSatisfy(code("OPERATION_ERROR"));
		expect(String((error as Error).message)).not.toContain("secret");
		expect(await failing.why("task-1")).toMatchObject([
			{ state: "failed", phase: "gate", error: { code: "OPERATION_ERROR" } },
		]);

		const malformed = new NativeMemoryService({
			store: durableStore(),
			backend: hindsight.backend(),
			gate: async () => ({ retrieve: "yes", query: "rewritten" }) as never,
		});
		await expect(malformed.prepare({ query: "q", taskId: "task-2" })).rejects.toSatisfy(code("INVALID_GATE"));
		expect(await malformed.why("task-2")).toMatchObject([{ state: "failed", error: { code: "INVALID_GATE" } }]);
		expect(hindsight.captured).toEqual([]);
	});

	test("an unauthorized scope is denied before the gate or backend, and the denial is recorded", async () => {
		const hindsight = new FakeHindsight();
		const log: GateLog = [];
		const memory = new NativeMemoryService({
			store: durableStore(),
			backend: hindsight.backend({ session: scopes.session }),
			gate: gate(true, log),
		});
		await expect(memory.prepare({ query: "q", scope: "project", taskId: "task-1" })).rejects.toSatisfy(
			code("UNSUPPORTED_SCOPE"),
		);
		const [denial] = await memory.why("task-1");
		expect(denial).toMatchObject({
			kind: "prepare",
			scope: "project",
			state: "failed",
			phase: "gate",
			error: { code: "UNSUPPORTED_SCOPE" },
			queryHash: sha256("q"),
		});
		expect(denial.tags).toBeUndefined();
		expect(log).toEqual([]);
		expect(hindsight.captured).toEqual([]);
		// A malformed scope never reaches the journal at all.
		await expect(memory.prepare({ query: "q", scope: "admin" as never, taskId: "task-2" })).rejects.toSatisfy(
			code("UNSUPPORTED_SCOPE"),
		);
		expect(await memory.why("task-2")).toEqual([]);
	});

	test("a compatible repeat reuses the recorded decision; refresh, changed input and mutation re-query", async () => {
		const hindsight = new FakeHindsight();
		const log: GateLog = [];
		const memory = open(hindsight, durableStore(), gate(true, log));
		await memory.propose({ text: "fact one", evidence: [{ ref: "task:0" }] });
		const first = await memory.prepare({ query: "facts", taskId: "task-1" });
		const reused = await memory.prepare({ query: "facts", taskId: "task-1" });
		expect(reused).toEqual(first);
		expect(hindsight.recalls()).toHaveLength(1);
		expect(log.filter((request) => request.action === "recall")).toHaveLength(1);
		expect(await memory.why("task-1")).toHaveLength(1);

		const refreshed = await memory.prepare({ query: "facts", taskId: "task-1", refresh: true });
		expect(refreshed.operation.id).not.toBe(first.operation.id);
		await memory.prepare({ query: "other facts", taskId: "task-1" });
		await memory.prepare({ query: "facts", taskId: "task-1", scope: "project" });
		expect(hindsight.recalls().map((call) => [call.body!.query, call.body!.tags])).toEqual([
			["facts", scopes.session],
			["facts", scopes.session],
			["other facts", scopes.session],
			["facts", scopes.project],
		]);
		expect(await memory.why("task-1")).toHaveLength(4);

		// A dispatched write invalidates reuse: the next identical request searches again.
		await memory.propose({ text: "fact two", evidence: [{ ref: "task:9" }] });
		const afterWrite = await memory.prepare({ query: "facts", taskId: "task-1" });
		expect(hindsight.recalls()).toHaveLength(5);
		expect(afterWrite.results.map((item) => item.text)).toEqual(["fact one", "fact two"]);

		// Skip decisions are reused too, so the gate is not re-asked; a restart starts fresh.
		const skipLog: GateLog = [];
		const store = durableStore();
		const skipping = open(hindsight, store, gate(false, skipLog));
		await skipping.prepare({ query: "q", taskId: "task-s" });
		await skipping.prepare({ query: "q", taskId: "task-s" });
		expect(skipLog).toHaveLength(1);
		await open(hindsight, store, gate(false, skipLog)).prepare({ query: "q", taskId: "task-s" });
		expect(skipLog).toHaveLength(2);
		expect(hindsight.recalls()).toHaveLength(5);
	});
});
