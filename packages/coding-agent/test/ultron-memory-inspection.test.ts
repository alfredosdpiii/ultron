import { createHash } from "node:crypto";
import { describe, expect, test } from "vitest";
import { createSessionControl } from "../src/experimental/services/session-control-provider.ts";
import { type JsonValue, type MemoryBackend, NativeMemoryService } from "../src/ultron/memory.ts";

const scopes = { session: ["ultron:session:s1"], project: ["ultron:project:p1"], global: ["ultron:global:g1"] };

function service() {
	const calls = { recall: 0, get: 0 };
	let stored: JsonValue | undefined;
	const backend: MemoryBackend = {
		namespace: "fake://bank",
		scopeTags: scopes,
		async recall(request) {
			calls.recall += 1;
			const scope = request.tags.includes("ultron:project:p1") ? "project" : "session";
			return { results: [{ id: `${scope}-fact`, text: `${scope} evidence`, tags: request.tags }] };
		},
		async get() {
			calls.get += 1;
			return undefined;
		},
	};
	const memory = new NativeMemoryService({
		backend,
		gate: async () => ({ retrieve: true }),
		store: {
			read: async () => structuredClone(stored),
			write: async (next) => {
				stored = structuredClone(next);
			},
		},
	});
	return { memory, calls };
}

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");

describe("A29 memory inspector", () => {
	test("why shows the evidence that was injected, with no new search and no other task's scope", async () => {
		const { memory, calls } = service();
		const injected = await memory.prepare({ query: "launcher path", taskId: "task-a" });
		await memory.prepare({ query: "secret project detail", taskId: "task-b", scope: "project" });
		const searches = calls.recall;

		const why = await memory.why("task-a");
		expect(calls).toEqual({ recall: searches, get: 0 });
		expect(why).toHaveLength(1);
		expect(why[0]).toMatchObject({
			taskId: "task-a",
			scope: "session",
			state: "recalled",
			tags: scopes.session,
			references: injected.results.map((item) => ({ id: item.id, textHash: sha256(item.text) })),
		});
		// The recorded query is a hash; neither the other task's scope nor its query leaks.
		const text = JSON.stringify(why);
		expect(text).not.toContain("project");
		expect(text).not.toContain("launcher path");
		expect(text).not.toContain("secret project detail");
	});

	test("the inspector refuses requests that would search or change state", async () => {
		const requests: string[] = [];
		const control = createSessionControl({
			harness: {} as never,
			lane: {} as never,
			cwd: process.cwd(),
			inspect: async (request) => {
				requests.push(request);
				return [];
			},
		});
		for (const request of ["memory.prepare", "memory.forget", "memory.get", "agents.spawn", "refinements.activate"]) {
			await expect(control.inspect(request, {}, {} as never)).rejects.toThrow("Not an inspection request");
		}
		await expect(control.inspect("memory.why", { taskId: "task-a" }, {} as never)).resolves.toEqual([]);
		expect(requests).toEqual(["memory.why"]);
	});
});
