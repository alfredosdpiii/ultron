import { createHash } from "node:crypto";
import type { Context, JsonValue } from "@ultron/chord";
import { describe, expect, test } from "vitest";
import {
	type DurableDocumentStorage,
	type ExperimentRecord,
	NativeLocalServices,
} from "../src/ultron/local-services.ts";
import { MemoryError, NativeMemoryService } from "../src/ultron/memory.ts";
import { durableStore, FakeHindsight, gate } from "./ultron-fake-hindsight.ts";

const context = {} as Context;
const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");

function documents(): DurableDocumentStorage {
	const values = new Map<string, JsonValue>();
	return {
		get: async (key) => structuredClone(values.get(key)),
		set: async (key, value) => {
			values.set(key, structuredClone(value));
		},
		list: async (prefix) =>
			[...values].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => ({ key, value })),
	};
}

// Frozen task and configuration; the hash is computed from the content, not chosen by the caller.
const fixture = { task: "fix off-by-one in pager", files: { "src/pager.ts": "..." }, checks: ["npm test"] };
const config = { model: "provider/model-a", thinking: "medium", definitions: { "rlm-child": 1 } };
const fixtureHash = sha256(JSON.stringify(fixture));
const configHash = sha256(JSON.stringify(config));

describe("A33 experiment history includes all attempts and variants do not contaminate each other", () => {
	test("every attempt, including crashed, failed and incomplete ones, is recorded against the frozen fixture", async () => {
		const ledger = documents();
		const record = (run: Record<string, JsonValue>) =>
			new NativeLocalServices(ledger).handle("experiments.record", { run }, context) as Promise<ExperimentRecord>;
		const attempts: Record<string, JsonValue>[] = [
			{ variant: "baseline", attempt: 1, outcome: "incomplete", reason: "worker crashed" },
			{ variant: "baseline", attempt: 2, outcome: "failed", reason: "npm test failed" },
			{ variant: "baseline", attempt: 3, outcome: "passed" },
			{ variant: "candidate", attempt: 1, outcome: "incomplete", reason: "cancelled at budget" },
			{ variant: "candidate", attempt: 2, outcome: "passed" },
		];
		const recorded: ExperimentRecord[] = [];
		for (const attempt of attempts) recorded.push(await record({ ...attempt, fixtureHash, configHash }));

		// Restart: the ledger returns every attempt in order, with the same frozen hashes.
		const listed = (await new NativeLocalServices(ledger).handle(
			"experiments.list",
			{},
			context,
		)) as ExperimentRecord[];
		expect(listed).toEqual(recorded);
		expect(listed.map((run) => [run.variant, run.attempt, run.outcome])).toEqual(
			attempts.map((run) => [run.variant, run.attempt, run.outcome]),
		);
		expect(new Set(listed.map((run) => run.fixtureHash))).toEqual(new Set([sha256(JSON.stringify(fixture))]));
		expect(new Set(listed.map((run) => run.id)).size).toBe(attempts.length);

		// Records are append-only: no update or delete request exists, and an unknown outcome is refused.
		const services = new NativeLocalServices(ledger);
		for (const type of ["experiments.update", "experiments.delete"])
			await expect(services.handle(type, { id: listed[0].id }, context)).rejects.toThrow("Unknown local service");
		await expect(
			services.handle(
				"experiments.record",
				{ run: { variant: "baseline", fixtureHash, outcome: "crashed" } },
				context,
			),
		).rejects.toThrow("outcome");
		// Only runs on the same frozen fixture compare; a changed fixture cannot.
		await expect(
			services.handle("experiments.compare", { baseline: listed[2].id, candidate: listed[4].id }, context),
		).resolves.toMatchObject({ claim: "Observed runs only; no statistical superiority established" });
		const drifted = await record({
			variant: "candidate",
			attempt: 3,
			outcome: "passed",
			fixtureHash: sha256("edited"),
		});
		await expect(
			services.handle("experiments.compare", { baseline: listed[2].id, candidate: drifted.id }, context),
		).rejects.toThrow("Different fixtures");
		expect(((await services.handle("experiments.list", {}, context)) as unknown[]).length).toBe(6);
	});

	test("variants with isolated memory and refinement state never see each other's writes", async () => {
		const hindsight = new FakeHindsight(); // one shared Hindsight server, as in production
		const variant = (name: string) => {
			const tags = { session: [`ultron:session:experiment-${name}`] };
			const journal = durableStore();
			return {
				name,
				tags,
				journal,
				memory: new NativeMemoryService({ store: journal, backend: hindsight.backend(tags), gate: gate() }),
				refinements: new NativeLocalServices(documents()),
			};
		};
		const a = variant("a");
		const b = variant("b");

		// Variant A learns a lesson and activates a refinement.
		const lesson = await a.memory.propose({ text: "A-only lesson", evidence: [{ ref: "run:a-1" }] });
		await a.memory.get(lesson.memoryId!);
		const refinement = (await a.refinements.handle(
			"refinements.propose",
			{
				kind: "instruction",
				target: "instruction:rlm-child",
				baseVersion: 0,
				content: "A-only instruction",
				evidence: [{ run: "a-1" }],
			},
			context,
		)) as { id: string };
		await a.refinements.handle("refinements.activate", { id: refinement.id }, context);

		// Variant B sees none of it: no recalled memory, no active refinement.
		const recalledB = await b.memory.prepare({ query: "lesson", taskId: "b-1" });
		expect(recalledB.results).toEqual([]);
		expect(
			await b.refinements.handle(
				"refinements.current",
				{ kind: "instruction", target: "instruction:rlm-child" },
				context,
			),
		).toBeNull();
		const recalledA = await a.memory.prepare({ query: "lesson", taskId: "a-2" });
		expect(recalledA.results.map((item) => item.text)).toEqual(["A-only lesson"]);
		expect(hindsight.recalls().map((call) => call.body!.tags)).toEqual([b.tags.session, a.tags.session]);

		// B cannot borrow A's journal to read A's document: the handle is bound to A's scope tags.
		const borrowed = new NativeMemoryService({ store: a.journal, backend: hindsight.backend(b.tags), gate: gate() });
		await expect(borrowed.get(lesson.memoryId!)).rejects.toSatisfy(
			(error: unknown) => error instanceof MemoryError && error.code === "UNSUPPORTED_SCOPE",
		);
		// A journal from a different memory bank is refused outright.
		const otherBank = new NativeMemoryService({
			store: a.journal,
			backend: hindsight.backend(b.tags, "bank-b"),
			gate: gate(),
		});
		await expect(otherBank.list()).rejects.toSatisfy(
			(error: unknown) => error instanceof MemoryError && error.code === "BACKEND_MISMATCH",
		);

		// Each variant's recorded memory state names only its own scope and writes.
		const ledger = new NativeLocalServices(documents());
		for (const item of [a, b]) {
			const operations = await item.memory.list();
			await ledger.handle(
				"experiments.record",
				{
					run: {
						variant: item.name,
						fixtureHash,
						outcome: "passed",
						memory: { tags: item.tags.session, writes: operations.filter((op) => op.kind === "propose").length },
					},
				},
				context,
			);
		}
		const runs = (await ledger.handle("experiments.list", {}, context)) as ExperimentRecord[];
		expect(runs.map((run) => run.memory)).toEqual([
			{ tags: a.tags.session, writes: 1 },
			{ tags: b.tags.session, writes: 0 },
		]);
	});

	test("per-variant usage, including failed and incomplete attempts, reconciles with independent metering", async () => {
		const meter = new Map<string, number>(); // independent provider-side accounting
		const ledger = new NativeLocalServices(documents());
		const attempts = [
			{ variant: "baseline", outcome: "incomplete", usage: { root: 1200, descendants: 0, frames: 40, memory: 0 } },
			{ variant: "baseline", outcome: "failed", usage: { root: 3400, descendants: 900, frames: 40, memory: 0 } },
			{ variant: "baseline", outcome: "passed", usage: { root: 2800, descendants: 700, frames: 40, memory: 0 } },
			{ variant: "memory", outcome: "failed", usage: { root: 2600, descendants: 300, frames: 80, memory: 120 } },
			{ variant: "memory", outcome: "passed", usage: { root: 2100, descendants: 250, frames: 80, memory: 120 } },
		] as const;
		for (const attempt of attempts) {
			const total = Object.values(attempt.usage).reduce((sum: number, value) => sum + value, 0);
			meter.set(attempt.variant, (meter.get(attempt.variant) ?? 0) + total);
			await ledger.handle(
				"experiments.record",
				{ run: { ...attempt, fixtureHash, usage: { ...attempt.usage, total } } },
				context,
			);
		}
		// A usage record whose parts do not add up is refused rather than silently stored.
		await expect(
			ledger.handle(
				"experiments.record",
				{ run: { variant: "memory", fixtureHash, outcome: "failed", usage: { root: 10, frames: 5, total: 12 } } },
				context,
			),
		).rejects.toThrow("does not reconcile");
		await expect(
			ledger.handle(
				"experiments.record",
				{ run: { variant: "memory", fixtureHash, outcome: "failed", usage: { root: -1, total: -1 } } },
				context,
			),
		).rejects.toThrow("nonnegative");

		const runs = (await ledger.handle("experiments.list", {}, context)) as (ExperimentRecord & {
			usage: Record<string, number>;
		})[];
		expect(runs).toHaveLength(attempts.length);
		const perVariant = new Map<string, number>();
		for (const run of runs) perVariant.set(run.variant, (perVariant.get(run.variant) ?? 0) + run.usage.total);
		expect(perVariant).toEqual(meter);
		// Failed and incomplete attempts carry real cost and are part of each variant's total.
		const unsuccessful = runs
			.filter((run) => run.outcome !== "passed")
			.reduce((sum, run) => sum + run.usage.total, 0);
		expect(unsuccessful).toBe(1240 + 4340 + 3100);
	});
});
