import type { Context, JsonValue } from "@ultron/chord";
import { describe, expect, test } from "vitest";
import {
	type DurableDocumentStorage,
	type LocalServiceDocument,
	NativeLocalServices,
} from "../src/ultron/local-services.ts";

const context = {} as Context;

class MemoryDocuments implements DurableDocumentStorage {
	private readonly values = new Map<string, JsonValue>();

	async get(key: string, _context: Context): Promise<JsonValue | undefined> {
		const value = this.values.get(key);
		return value === undefined ? undefined : structuredClone(value);
	}

	async set(key: string, value: JsonValue, _context: Context): Promise<void> {
		this.values.set(key, structuredClone(value));
	}

	async list(prefix: string, _context: Context): Promise<LocalServiceDocument[]> {
		return [...this.values.entries()]
			.filter(([key]) => key.startsWith(prefix))
			.map(([key, value]) => ({ key, value: structuredClone(value) }));
	}
}

function service(
	documents = new MemoryDocuments(),
	options: ConstructorParameters<typeof NativeLocalServices>[1] = {},
) {
	return { documents, host: new NativeLocalServices(documents, options) };
}

function input(overrides: Record<string, JsonValue> = {}): Record<string, JsonValue> {
	return {
		kind: "skill",
		target: "skill:review",
		baseVersion: 0,
		content: "Check boundary cases before suggesting a fix.",
		evidence: [{ test: "boundary regression", outcome: "passed" }],
		...overrides,
	};
}

describe("native local services", () => {
	test("stores refinements through injected documents and enforces versions without approval", async () => {
		const { documents, host } = service();
		const proposed = await host.handle("refinements.propose", input(), context);
		expect(proposed).toMatchObject({ state: "proposed", version: null, scope: "session" });
		expect(await host.handle("refinements.current", { kind: "skill", target: "skill:review" }, context)).toBeNull();

		const activated = await host.handle("refinements.activate", { id: (proposed as { id: string }).id }, context);
		expect(activated).toMatchObject({ state: "active", version: 1 });
		expect(
			await host.handle("refinements.current", { kind: "skill", target: "skill:review" }, context),
		).toMatchObject({
			id: (proposed as { id: string }).id,
			state: "active",
		});
		expect(await documents.get("ultron.refinements", context)).toMatchObject({ formatVersion: 1 });

		const competing = await host.handle(
			"refinements.propose",
			input({ content: "An alternative revision." }),
			context,
		);
		await expect(
			host.handle("refinements.activate", { id: (competing as { id: string }).id }, context),
		).rejects.toThrow("Stale base version");
		expect(await host.handle("refinements.get", { id: (competing as { id: string }).id }, context)).toMatchObject({
			state: "proposed",
		});
	});

	test("requires evidence, validates activation, and preserves rollback history", async () => {
		let calls = 0;
		const { host } = service(undefined, {
			validate: (_kind, content) => {
				calls += 1;
				if (typeof content !== "object" || content === null || Array.isArray(content)) throw new Error("schema");
				return Object.hasOwn(content, "instructions");
			},
		});
		const noEvidence = await host.handle("refinements.propose", input({ evidence: [] }), context);
		await expect(
			host.handle("refinements.activate", { id: (noEvidence as { id: string }).id }, context),
		).rejects.toThrow("nonempty evidence");
		expect(calls).toBe(0);

		const invalid = await host.handle(
			"refinements.propose",
			input({ kind: "agent", target: "agent:review", content: { strategy: "deterministic" } }),
			context,
		);
		await expect(
			host.handle("refinements.activate", { id: (invalid as { id: string }).id }, context),
		).rejects.toThrow("validation failed");
		expect(calls).toBe(1);

		const valid = await host.handle(
			"refinements.propose",
			input({ kind: "agent", target: "agent:review", content: { instructions: "Review." } }),
			context,
		);
		const active = await host.handle("refinements.activate", { id: (valid as { id: string }).id }, context);
		const rolledBack = await host.handle("refinements.rollback", { id: (valid as { id: string }).id }, context);
		expect(active).toMatchObject({ state: "active", version: 1 });
		expect(rolledBack).toMatchObject({ state: "rolled_back" });
		expect(calls).toBe(2);
		expect(
			(
				(await host.handle("refinements.get", { id: (valid as { id: string }).id }, context)) as {
					history: unknown[];
				}
			).history,
		).toHaveLength(3);
	});

	test("rejects path-like and protected refinement targets without touching documents", async () => {
		const { documents, host } = service();
		for (const target of [
			"/tmp/AGENTS.md",
			"skill:AGENTS.md",
			"policy:rules",
			"skill:../review",
			"skill:review:policy",
		]) {
			await expect(host.handle("refinements.propose", input({ target }), context)).rejects.toThrow(
				/target|Forbidden|namespace/i,
			);
		}
		expect(await documents.list("", context)).toEqual([]);
	});

	test("deduplicates and reads artifacts with integrity and range checks", async () => {
		const { documents, host } = service();
		const put = await host.handle(
			"artifacts.put",
			{ text: "x".repeat(100_000), options: { label: "fixture" } },
			context,
		);
		expect(put).toMatchObject({ bytes: 100_000, mediaType: "text/plain", label: "fixture" });
		expect((put as { preview: string }).preview).toHaveLength(2_048);
		const read = await host.handle(
			"artifacts.read",
			{ id: (put as { id: string }).id, options: { offset: 99_990, length: 10 } },
			context,
		);
		expect(read).toMatchObject({ text: "xxxxxxxxxx", offset: 99_990, total: 100_000 });
		expect(await host.handle("artifacts.list", {}, context)).toEqual([
			{ id: (put as { id: string }).id, bytes: 100_000, mediaType: "text/plain", label: "fixture" },
		]);
		const key = `ultron.artifacts/${(put as { id: string }).id}`;
		const stored = await documents.get(key, context);
		if (stored && typeof stored === "object" && !Array.isArray(stored)) {
			stored.text = "tampered";
			await documents.set(key, stored, context);
		}
		await expect(host.handle("artifacts.read", { id: (put as { id: string }).id }, context)).rejects.toThrow(
			"integrity",
		);
	});

	test("records and compares only matched experiment fixtures", async () => {
		const { host } = service();
		const baseline = await host.handle(
			"experiments.record",
			{ run: { variant: "old", fixtureHash: "same", outcome: "failed" } },
			context,
		);
		const candidate = await host.handle(
			"experiments.record",
			{ run: { variant: "new", fixtureHash: "same", outcome: "passed" } },
			context,
		);
		const comparison = await host.handle(
			"experiments.compare",
			{ baseline: (baseline as { id: string }).id, candidate: (candidate as { id: string }).id },
			context,
		);
		expect(comparison).toMatchObject({ claim: "Observed runs only; no statistical superiority established" });
		const other = await host.handle(
			"experiments.record",
			{ run: { variant: "other", fixtureHash: "different", outcome: "passed" } },
			context,
		);
		await expect(
			host.handle(
				"experiments.compare",
				{ baseline: (baseline as { id: string }).id, candidate: (other as { id: string }).id },
				context,
			),
		).rejects.toThrow("Different fixtures");
	});
});
