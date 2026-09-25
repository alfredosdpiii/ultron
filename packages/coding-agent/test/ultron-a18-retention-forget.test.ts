import { describe, expect, test } from "vitest";
import { MemoryError, NativeMemoryService } from "../src/ultron/memory.ts";
import { durableStore, FakeHindsight, gate, open, scopes } from "./ultron-fake-hindsight.ts";

const code = (expected: string) => (error: unknown) => error instanceof MemoryError && error.code === expected;

describe("A18 ingestion acceptance is distinct from completed retention; forgetting persists", () => {
	test("accepted-but-pending stays distinct from stored through receipt, backend failure and restart", async () => {
		const hindsight = new FakeHindsight();
		const store = durableStore();
		hindsight.receiptState = "pending";
		const accepted = await open(hindsight, store).propose({ text: "fact A", evidence: [{ ref: "task:1" }] });
		expect(accepted).toMatchObject({ state: "accepted", operationIds: [accepted.id] });

		// Restart: acceptance is still only acceptance, and no document read is attempted.
		let memory = open(hindsight, store);
		await expect(memory.get(accepted.memoryId!)).resolves.toMatchObject({ state: "accepted", content: null });
		expect(hindsight.calls("GET", `/documents/${accepted.memoryId}`)).toBe(0);

		// The receipt endpoint is down: the read fails, and the claim is neither promoted nor demoted.
		hindsight.failNext.add("GET /operations/");
		await expect(memory.get(accepted.memoryId!)).rejects.toSatisfy(code("HTTP_ERROR"));
		memory = open(hindsight, store);
		const claim = (await memory.list()).find((op) => op.id === accepted.id)!;
		expect(claim.state).toBe("accepted");

		// Only a completed receipt establishes retention.
		hindsight.operations.set(accepted.id, "completed");
		await expect(memory.get(accepted.memoryId!)).resolves.toMatchObject({ state: "stored", content: "fact A" });
		expect((await open(hindsight, store).list()).find((op) => op.id === accepted.id)).toMatchObject({
			state: "stored",
			receipts: [{ id: accepted.id, status: "completed" }],
		});

		// A failed background ingestion is reported as failed, never as stored.
		hindsight.receiptState = "failed";
		const failed = await memory.propose({ text: "fact B", evidence: [{ ref: "task:2" }] });
		expect(failed.state).toBe("accepted");
		await expect(open(hindsight, store).get(failed.memoryId!)).resolves.toMatchObject({
			state: "failed",
			content: null,
		});

		// A write whose response is lost is unknown: not accepted, not stored, and never replayed.
		hindsight.receiptState = "completed";
		hindsight.failNext.add("POST /memories");
		await expect(memory.propose({ text: "fact C", evidence: [{ ref: "task:3" }] })).rejects.toSatisfy(
			code("HTTP_ERROR"),
		);
		const retainsBefore = hindsight.retains().length;
		memory = open(hindsight, store);
		const lost = (await memory.list()).find((op) => op.kind === "propose" && op.state === "unknown")!;
		expect(lost).toMatchObject({ phase: "backend", error: { code: "HTTP_ERROR" } });
		await expect(
			memory.correct(lost.memoryId!, { text: "fact C'", evidence: [{ ref: "task:4" }] }),
		).rejects.toSatisfy(code("BUSY"));
		expect(hindsight.retains()).toHaveLength(retainsBefore);
	});

	test("a crash during dispatch reopens as unknown with no replay", async () => {
		const hindsight = new FakeHindsight();
		const store = durableStore();
		const backend = hindsight.backend();
		let release!: () => void;
		const retain = backend.retain!.bind(backend);
		backend.retain = async (request, signal) => {
			await new Promise<void>((resolve) => {
				release = resolve;
			});
			return retain(request, signal);
		};
		const crashed = new NativeMemoryService({ store, backend, gate: gate() });
		const pending = crashed.propose({ text: "in flight", evidence: [{ ref: "task:1" }] });
		await new Promise((resolve) => setTimeout(resolve, 5));
		const snapshot = structuredClone(store.value); // durable state at the moment the owner died

		const restartedStore = durableStore();
		restartedStore.value = snapshot;
		const restarted = open(hindsight, restartedStore);
		const [op] = (await restarted.list()).filter((item) => item.kind === "propose");
		expect(op).toMatchObject({ state: "unknown", phase: "backend", error: { code: "OWNER_ENDED" } });
		expect(hindsight.retains()).toHaveLength(0);
		release();
		await pending;
	});

	test("forget persists across restart and outranks a backend that still returns the item", async () => {
		const hindsight = new FakeHindsight();
		const store = durableStore();
		let memory = open(hindsight, store);
		const kept = await memory.propose({ text: "keep me", evidence: [{ ref: "task:1" }] });
		const doomed = await memory.propose({ text: "secret-ish launcher detail", evidence: [{ ref: "task:2" }] });
		await memory.get(kept.memoryId!);
		await memory.get(doomed.memoryId!);
		const forgotten = await memory.forget(doomed.memoryId!);
		expect(forgotten).toMatchObject({ kind: "forget", state: "forgotten", phase: "backend" });
		expect(hindsight.calls("DELETE", `/documents/${doomed.memoryId}`)).toBe(1);

		// The backend misbehaves: its index still returns the deleted unit and the document reappears.
		const document = { id: doomed.memoryId!, content: "secret-ish launcher detail", tags: scopes.session };
		hindsight.stale.push({
			id: "ghost-unit",
			text: document.content,
			tags: scopes.session,
			type: "world",
			document_id: document.id,
			metadata: { ultron_operation: doomed.id, ultron_evidence_class: "hypothesis" },
		});
		hindsight.documents.set(document.id, {
			...document,
			observationScopes: [scopes.session],
			metadata: { ultron_operation: doomed.id, ultron_evidence_class: "hypothesis" },
		});

		// Restart: the local forget record wins everywhere.
		memory = open(hindsight, store);
		const gets = hindsight.calls("GET", `/documents/${doomed.memoryId}`);
		await expect(memory.get(doomed.memoryId!)).rejects.toSatisfy(code("FORGOTTEN"));
		expect(hindsight.calls("GET", `/documents/${doomed.memoryId}`)).toBe(gets);
		await expect(
			memory.correct(doomed.memoryId!, { text: "resurrect", evidence: [{ ref: "task:3" }] }),
		).rejects.toSatisfy(code("FORGOTTEN"));
		await expect(memory.forget(doomed.memoryId!)).rejects.toSatisfy(code("FORGOTTEN"));
		expect(hindsight.calls("DELETE", `/documents/${doomed.memoryId}`)).toBe(1);

		const prepared = await memory.prepare({ query: "launcher", taskId: "task-after" });
		expect(prepared.results.map((item) => item.memoryId)).toEqual([kept.memoryId]);
		expect(prepared.context).not.toContain("secret-ish");
		const [why] = await memory.why("task-after");
		expect(why.excluded).toEqual([
			{ id: expect.stringContaining(doomed.memoryId!), memoryId: doomed.memoryId, reason: "forgotten" },
			{ id: "ghost-unit", memoryId: doomed.memoryId, reason: "forgotten" },
		]);
		expect(JSON.stringify(store.value)).not.toContain("secret-ish");
	});

	test("a forget whose delete fails is unknown, not forgotten, and still withholds the item after restart", async () => {
		const hindsight = new FakeHindsight();
		const store = durableStore();
		let memory = open(hindsight, store);
		const item = await memory.propose({ text: "please forget", evidence: [{ ref: "task:1" }] });
		await memory.get(item.memoryId!);
		hindsight.failNext.add("DELETE /documents/");
		await expect(memory.forget(item.memoryId!)).rejects.toSatisfy(code("HTTP_ERROR"));

		memory = open(hindsight, store);
		const forget = (await memory.list()).find((op) => op.kind === "forget")!;
		expect(forget).toMatchObject({ state: "unknown", phase: "backend" });
		// Reporting honesty: the read says unknown with no content, not forgotten and not stored.
		await expect(memory.get(item.memoryId!)).resolves.toMatchObject({ state: "unknown", content: null });
		await expect(memory.correct(item.memoryId!, { text: "x", evidence: [{ ref: "t" }] })).rejects.toSatisfy(
			code("BUSY"),
		);
		// The document still exists in the backend, but the requested forget keeps it out of context.
		expect(hindsight.documents.has(item.memoryId!)).toBe(true);
		const prepared = await memory.prepare({ query: "anything", taskId: "task-x" });
		expect(prepared.results).toEqual([]);
		expect((await memory.why("task-x"))[0].excluded).toEqual([
			{ id: expect.any(String), memoryId: item.memoryId, reason: "forgotten" },
		]);
	});
});
