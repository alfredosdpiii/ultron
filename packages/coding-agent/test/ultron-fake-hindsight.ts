import {
	createHindsightBackend,
	type JsonValue,
	type MemoryBackend,
	type MemoryGate,
	type MemoryScopeTags,
	type MemoryStore,
	NativeMemoryService,
} from "../src/ultron/memory.ts";

/**
 * An in-process fake of the Hindsight 0.9.2 HTTP endpoints used by the native adapter.
 * Every request goes through createHindsightBackend and a captured fetch, so tests see the
 * exact wire bodies. Consolidation follows Hindsight's documented rule: observations form
 * per explicit observation scope and carry exactly that scope's tags.
 */
export type FakeDocument = {
	id: string;
	content: string;
	tags: string[];
	observationScopes: string[][];
	metadata: Record<string, string>;
};
export type FakeUnit = {
	id: string;
	text: string;
	tags: string[];
	type: "world" | "observation";
	document_id: string | null;
	metadata: Record<string, string> | null;
};
export type Captured = { method: string; path: string; body?: Record<string, unknown> };

export const scopes = {
	session: ["ultron:session:s1"],
	project: ["ultron:project:p1"],
	global: ["ultron:global:g1"],
} satisfies MemoryScopeTags;

const sameSet = (left: string[], right: string[]) =>
	left.length === right.length && left.every((tag) => right.includes(tag));

export class FakeHindsight {
	readonly captured: Captured[] = [];
	readonly documents = new Map<string, FakeDocument>();
	/** Units that survive deletion: models a stale index that still returns forgotten or replaced text. */
	readonly stale: FakeUnit[] = [];
	readonly observations: FakeUnit[] = [];
	readonly operations = new Map<string, "pending" | "processing" | "completed" | "failed" | "cancelled">();
	/** When set, new retain receipts start in this state instead of completed. */
	receiptState: "pending" | "completed" | "failed" = "completed";
	/** Makes the next request of this path fail with HTTP 503. */
	failNext = new Set<string>();
	/** Fault injection: consolidation that ignores observation scopes and merges across them. */
	mergeAcrossScopes = false;
	/** Fault injection: recall returns consolidated observations without applying the tag filter. */
	leakObservations = false;
	private counter = 0;

	calls(method: string, suffix: string): number {
		return this.captured.filter((call) => call.method === method && call.path.endsWith(suffix)).length;
	}
	recalls(): Captured[] {
		return this.captured.filter((call) => call.path.endsWith("/memories/recall"));
	}
	retains(): Captured[] {
		return this.captured.filter((call) => call.method === "POST" && call.path.endsWith("/memories"));
	}

	backend(scopeTags: MemoryScopeTags = scopes, bankId = "bank-1"): MemoryBackend {
		return createHindsightBackend({ baseUrl: "http://hindsight.test", bankId, scopeTags, fetch: this.fetch });
	}

	/** Hindsight's background consolidation, run on demand. */
	consolidate(): void {
		this.observations.length = 0;
		const groups = new Map<string, { tags: string[]; texts: string[] }>();
		for (const document of this.documents.values()) {
			const tags = this.mergeAcrossScopes
				? [...new Set([...this.documents.values()].flatMap((item) => item.tags))]
				: document.observationScopes[0];
			const key = JSON.stringify([...tags].sort());
			const group = groups.get(key) ?? { tags, texts: [] };
			group.texts.push(document.content);
			groups.set(key, group);
		}
		for (const group of groups.values()) {
			this.observations.push({
				id: `observation-${++this.counter}`,
				text: `Consolidated: ${group.texts.join(" | ")}`,
				tags: [...group.tags],
				type: "observation",
				document_id: null,
				metadata: null,
			});
		}
	}

	private units(): FakeUnit[] {
		const live = [...this.documents.values()].map(
			(document): FakeUnit => ({
				id: `unit-${document.id}-${document.metadata.ultron_operation ?? "x"}`,
				text: document.content,
				tags: [...document.tags],
				type: "world",
				document_id: document.id,
				metadata: { ...document.metadata },
			}),
		);
		return [...live, ...this.stale, ...this.observations];
	}

	readonly fetch: typeof globalThis.fetch = async (input, init) => {
		const url = new URL(String(input));
		const path = url.pathname.replace(/^\/v1\/default\/banks\/[^/]+/, "");
		const method = init?.method ?? "GET";
		const body = typeof init?.body === "string" ? (JSON.parse(init.body) as Record<string, unknown>) : undefined;
		this.captured.push({ method, path, ...(body ? { body } : {}) });
		const reply = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
		const key = `${method} ${path}`;
		for (const failure of this.failNext) {
			if (key.startsWith(failure)) {
				this.failNext.delete(failure);
				return reply({ detail: "unavailable" }, 503);
			}
		}
		if (method === "POST" && path === "/memories/recall") {
			const tags = body!.tags as string[];
			return reply({
				results: this.units().filter(
					(unit) => sameSet(unit.tags, tags) || (this.leakObservations && unit.type === "observation"),
				),
			});
		}
		if (method === "POST" && path === "/memories") {
			const item = (body!.items as Record<string, unknown>[])[0];
			const id = item.document_id as string;
			const previous = this.documents.get(id);
			// update_mode "replace" drops the previous version's units; tags come from the request.
			if (previous && item.update_mode !== "replace") throw new Error("fake supports replace only");
			this.documents.set(id, {
				id,
				content: item.content as string,
				tags: item.tags as string[],
				observationScopes: item.observation_scopes as string[][],
				metadata: (item.metadata as Record<string, string>) ?? {},
			});
			const operationId = body!.operation_id as string;
			this.operations.set(operationId, this.receiptState);
			return reply({ success: true, async: true, operation_id: operationId });
		}
		const operation = /^\/operations\/(.+)$/.exec(path);
		if (method === "GET" && operation) {
			const id = decodeURIComponent(operation[1]);
			return reply({ operation_id: id, status: this.operations.get(id) ?? "not_found" });
		}
		const document = /^\/documents\/(.+)$/.exec(path);
		if (document) {
			const id = decodeURIComponent(document[1]);
			const found = this.documents.get(id);
			if (method === "GET")
				return found
					? reply({ id, original_text: found.content, tags: found.tags })
					: reply({ detail: "not found" }, 404);
			if (method === "DELETE") {
				this.documents.delete(id);
				return reply({ success: true, document_id: id, memory_units_deleted: found ? 1 : 0 });
			}
		}
		return reply({ detail: "unsupported" }, 404);
	};
}

/** A durable journal shared by every service instance that reopens it (a fresh session or restart). */
export function durableStore(): MemoryStore & { value: JsonValue | undefined } {
	return {
		value: undefined,
		async read() {
			return structuredClone(this.value);
		},
		async write(value) {
			this.value = structuredClone(value);
		},
	};
}

export type GateLog = Parameters<MemoryGate>[0][];

export function gate(recall = true, log: GateLog = []): MemoryGate {
	return async (request) => {
		log.push(structuredClone(request));
		return request.action === "recall" ? { retrieve: recall, probability: recall ? 0.9 : 0.1 } : { action: "keep" };
	};
}

export function open(hindsight: FakeHindsight, store: MemoryStore, memoryGate: MemoryGate = gate()) {
	return new NativeMemoryService({ store, backend: hindsight.backend(), gate: memoryGate });
}
