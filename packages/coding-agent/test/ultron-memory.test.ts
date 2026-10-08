import { describe, expect, test } from "vitest";
import {
	createHindsightBackend,
	type JsonValue,
	type MemoryBackend,
	MemoryError,
	type MemoryGate,
	type MemoryOperation,
	type MemoryStore,
	NativeMemoryService,
} from "../src/ultron/memory.ts";

type Document = { id: string; original_text: string | null; tags: string[] };

type Fixture = {
	service: NativeMemoryService;
	store: MemoryStore & { value: JsonValue | undefined };
	backend: MemoryBackend;
	calls: { recall: number; retain: number; get: number; delete: number; operation: number };
	documents: Map<string, Document>;
	operationStatuses: Map<string, "pending" | "processing" | "completed" | "failed" | "cancelled" | "not_found">;
};

const scopes = {
	session: ["ultron:session:s1"],
	project: ["ultron:project:p1"],
	global: ["ultron:global:g1"],
};

function fixture(
	gate: MemoryGate = async (request) => (request.action === "recall" ? { retrieve: true } : { action: "keep" }),
): Fixture {
	const documents = new Map<string, Document>();
	const operationStatuses = new Map<
		string,
		"pending" | "processing" | "completed" | "failed" | "cancelled" | "not_found"
	>();
	const calls = { recall: 0, retain: 0, get: 0, delete: 0, operation: 0 };
	const store: MemoryStore & { value: JsonValue | undefined } = {
		value: undefined,
		async read() {
			return structuredClone(this.value);
		},
		async write(value) {
			this.value = structuredClone(value);
		},
	};
	const backend: MemoryBackend = {
		namespace: "fake://bank",
		scopeTags: scopes,
		async recall(request) {
			calls.recall += 1;
			return { results: [{ id: "recalled-1", text: "old fact", tags: request.tags }] };
		},
		async retain(request) {
			calls.retain += 1;
			const item = request.items[0];
			documents.set(item.document_id, { id: item.document_id, original_text: item.content, tags: item.tags });
			operationStatuses.set(request.operation_id, "completed");
			return { success: true, async: true, operation_id: request.operation_id };
		},
		async operation(operationId) {
			calls.operation += 1;
			return { operation_id: operationId, status: operationStatuses.get(operationId) ?? "not_found" };
		},
		async get(id) {
			calls.get += 1;
			return documents.get(id);
		},
		async delete(id) {
			calls.delete += 1;
			documents.delete(id);
			return { success: true, document_id: id };
		},
	};
	return {
		service: new NativeMemoryService({ store, backend, gate }),
		store,
		backend,
		calls,
		documents,
		operationStatuses,
	};
}

function expectMemoryError(code: string) {
	return (error: unknown) => error instanceof MemoryError && error.code === code;
}

describe("NativeMemoryService", () => {
	test("requires a gate and concrete scope tags, and a skipped recall does not call the backend", async () => {
		const value = fixture(async () => ({ retrieve: false }));
		await expect(value.service.prepare({ query: "math", scope: "session", taskId: "task-1" })).resolves.toMatchObject(
			{
				results: [],
			},
		);
		expect(value.calls.recall).toBe(0);
		expect((await value.service.why("task-1")).at(-1)).toMatchObject({ state: "skipped", gate: { retrieve: false } });
		await expect(
			value.service.prepare({ query: "x", scope: "session", taskId: "task-2" }, undefined),
		).resolves.toBeDefined();
		const invalid = fixture();
		invalid.backend.scopeTags = { session: ["session"] };
		await expect(
			Promise.resolve().then(
				() =>
					new NativeMemoryService({
						store: invalid.store,
						backend: invalid.backend,
						gate: async () => ({ retrieve: true }),
					}),
			),
		).rejects.toSatisfy(expectMemoryError("UNSUPPORTED_SCOPE"));
	});

	test("sends exact tags and refuses recalled documents outside the requested scope", async () => {
		const value = fixture();
		const prepared = await value.service.prepare({ query: "prior work", scope: "session", taskId: "task-1" });
		expect(prepared.results[0]).toMatchObject({ id: "recalled-1", tags: scopes.session });
		expect(value.calls.recall).toBe(1);
		const bad = fixture();
		bad.backend.recall = async () => ({ results: [{ id: "outside", text: "wrong", tags: scopes.project }] });
		await expect(bad.service.prepare({ query: "x", scope: "session", taskId: "task-2" })).rejects.toSatisfy(
			expectMemoryError("INVALID_RESPONSE"),
		);
	});

	test("does not claim an async write is stored until a receipt completes", async () => {
		const value = fixture();
		const accepted = await value.service.propose({ text: "fact", evidence: [{ ref: "task:1" }] });
		expect(accepted.state).toBe("accepted");
		const id = accepted.memoryId!;
		value.operationStatuses.set(accepted.operationIds![0], "pending");
		await expect(value.service.get(id)).resolves.toMatchObject({ state: "accepted", content: null });
		expect(value.calls.get).toBe(0);
		value.operationStatuses.set(accepted.operationIds![0], "completed");
		await expect(value.service.get(id)).resolves.toMatchObject({ state: "stored", content: "fact" });
		expect(value.calls.get).toBe(1);
	});

	test("requires evidence with exact fields, owns handles, and does not preserve text in the store", async () => {
		const value = fixture();
		await expect(value.service.propose({ text: "fact", evidence: [] })).rejects.toSatisfy(
			expectMemoryError("INVALID_INPUT"),
		);
		await expect(
			value.service.propose({ text: "fact", evidence: [{ ref: "x", extra: "no" }] as never }),
		).rejects.toSatisfy(expectMemoryError("INVALID_INPUT"));
		await expect(value.service.get("not-owned")).rejects.toSatisfy(expectMemoryError("UNKNOWN_MEMORY"));
		const proposed = await value.service.propose({ text: "private fact", evidence: [{ ref: "source" }] });
		const persisted = JSON.stringify(value.store.value);
		expect(persisted).not.toContain("private fact");
		expect(persisted).toContain('"ref":"source"');
		await expect(value.service.correct("not-owned", { text: "x", evidence: [{ ref: "source" }] })).rejects.toSatisfy(
			expectMemoryError("UNKNOWN_MEMORY"),
		);
		expect(proposed.memoryId).toBeTypeOf("string");
	});

	test("an interrupted backend mutation becomes unknown and cannot be edited or replayed", async () => {
		const value = fixture();
		let release: (() => void) | undefined;
		value.backend.retain = async () => {
			await new Promise<void>((resolve) => {
				release = resolve;
			});
			return { success: true, async: true, operation_id: "receipt-1" };
		};
		const controller = new AbortController();
		const pending = value.service.propose({ text: "fact", evidence: [{ ref: "source" }] }, controller.signal);
		await new Promise<void>((resolve) => setImmediate(resolve));
		controller.abort();
		release?.();
		await expect(pending).rejects.toSatisfy(expectMemoryError("ABORTED"));
		const operations = await value.service.list();
		const accepted = operations.find(
			(operation) => operation.kind === "propose" && operation.phase === "backend",
		) as MemoryOperation;
		expect(accepted.state).toBe("unknown");
		await expect(
			value.service.correct(accepted.memoryId!, { text: "replacement", evidence: [{ ref: "source" }] }),
		).rejects.toSatisfy(expectMemoryError("BUSY"));
		expect(value.calls.retain).toBe(0);
	});

	test("Hindsight adapter uses bounded, explicit HTTP configuration", async () => {
		const requests: { url: string; signal: AbortSignal; body?: string }[] = [];
		const fetcher: typeof fetch = async (input, init) => {
			if (!init?.signal) throw new Error("missing signal");
			requests.push({
				url: String(input),
				signal: init.signal,
				body: typeof init.body === "string" ? init.body : undefined,
			});
			return new Response(JSON.stringify({ results: [] }), { status: 200, headers: { "content-length": "15" } });
		};
		const backend = createHindsightBackend({
			baseUrl: "https://hindsight.example/api",
			bankId: "bank/1",
			scopeTags: scopes,
			fetch: fetcher,
			timeoutMs: 1000,
			maxResponseBytes: 1024,
		});
		await backend.recall!({
			query: "x",
			tags: scopes.session,
			tags_match: "exact",
			types: ["world", "experience", "observation"],
			budget: "mid",
			max_tokens: 4096,
			trace: false,
		});
		expect(requests[0].url).toBe("https://hindsight.example/api/v1/default/banks/bank%2F1/memories/recall");
		expect(requests[0].signal).toBeInstanceOf(AbortSignal);
	});
});
