import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Context, JsonValue } from "@earendil-works/chord";
import { afterEach, describe, expect, test } from "vitest";
import {
	ArtifactQuotaError,
	type DurableDocumentStorage,
	type LocalServiceDocument,
	NativeLocalServices,
} from "../src/ultron/local-services.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { KernelPool, KernelPoolCapacityError } from "../src/ultron/rlm/kernel-pool.ts";

// A43: resource release/eviction respects live references, evidence retention, and quotas.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const context = {} as Context;
const directories: string[] = [];
const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
	for (const cleanup of cleanups.splice(0)) await cleanup();
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function scratch(): string {
	const directory = mkdtempSync(join(tmpdir(), "ultron-a43-"));
	directories.push(directory);
	return directory;
}

function clock() {
	let now = 1_000;
	return {
		now: () => now,
		advance: (ms: number) => {
			now += ms;
		},
	};
}

function pool(options: { maxLive: number; idleTtlMs?: number; snapshotPath?: (lane: string) => string | undefined }) {
	const time = clock();
	const created: Record<string, RlmKernel[]> = {};
	const instance = new KernelPool<RlmKernel>({
		maxLive: options.maxLive,
		idleTtlMs: options.idleTtlMs ?? 60_000,
		now: time.now,
		snapshotPath: options.snapshotPath,
		create: (lane) => {
			const kernel = new RlmKernel(
				{ cwd: process.cwd(), runtimePath, snapshotPath: options.snapshotPath?.(lane) },
				() => null,
			);
			created[lane] = [...(created[lane] ?? []), kernel];
			return kernel;
		},
	});
	cleanups.push(async () => {
		await instance.close();
		await Promise.all(
			Object.values(created)
				.flat()
				.map((kernel) => kernel.shutdown()),
		);
	});
	return { pool: instance, time, created };
}

class MemoryDocuments implements DurableDocumentStorage {
	readonly values = new Map<string, JsonValue>();
	async get(key: string): Promise<JsonValue | undefined> {
		const value = this.values.get(key);
		return value === undefined ? undefined : structuredClone(value);
	}
	async set(key: string, value: JsonValue): Promise<void> {
		this.values.set(key, structuredClone(value));
	}
	async list(prefix: string): Promise<LocalServiceDocument[]> {
		return [...this.values.entries()]
			.filter(([key]) => key.startsWith(prefix))
			.map(([key, value]) => ({ key, value: structuredClone(value) }));
	}
}

type Put = { id: string; bytes: number };

describe("A43 kernel release and idle eviction", () => {
	test("idle eviction skips a running cell and a pinned lane, and evicts idle unpinned kernels after the TTL", async () => {
		const { pool: kernels, time } = pool({ maxLive: 4, idleTtlMs: 30_000 });
		const running = await kernels.acquire("running");
		const cell = running.kernel.execute("import asyncio\nawait asyncio.sleep(0.3)\n'done'");
		await kernels.use("pinned", (kernel) => kernel.execute("kept = 1"));
		kernels.pin("pinned", "checkpoint:pinned@3");
		await kernels.use("idle", (kernel) => kernel.execute("scratch = 2"));

		time.advance(29_999);
		expect(await kernels.sweep()).toEqual([]);
		time.advance(1);
		const swept = await kernels.sweep();
		expect(swept).toEqual([
			expect.objectContaining({ lane: "idle", reason: "idle", evicted: true, restorable: "none" }),
		]);
		expect(
			kernels
				.stats()
				.lanes.map((lane) => lane.lane)
				.sort(),
		).toEqual(["pinned", "running"]);

		// Explicit eviction is refused, and recorded, while a cell runs or a holder pins the lane.
		expect(await kernels.evict("running")).toMatchObject({ evicted: false, refused: "running" });
		expect(await kernels.evict("pinned")).toMatchObject({ evicted: false, refused: "pinned" });
		expect(await cell).toMatchObject({ status: "ok", result: "'done'" });
		expect(await kernels.use("pinned", (kernel) => kernel.execute("kept"))).toMatchObject({ result: "1" });

		running.release();
		kernels.unpin("pinned", "checkpoint:pinned@3");
		time.advance(30_000);
		const later = await kernels.sweep();
		expect(later.map((record) => [record.lane, record.evicted]).sort()).toEqual([
			["pinned", true],
			["running", true],
		]);
		expect(kernels.stats().live).toBe(0);
		expect(kernels.evictions.map((record) => [record.lane, record.evicted, record.refused ?? null])).toEqual([
			["idle", true, null],
			["running", false, "running"],
			["pinned", false, "pinned"],
			["running", true, null],
			["pinned", true, null],
		]);
	});

	test("capacity evicts the least recently used idle kernel and otherwise fails with a typed result", async () => {
		const { pool: kernels, time, created } = pool({ maxLive: 2 });
		await kernels.use("first", (kernel) => kernel.execute("1"));
		time.advance(10);
		await kernels.use("second", (kernel) => kernel.execute("2"));
		time.advance(10);
		await kernels.use("third", (kernel) => kernel.execute("3"));
		expect(kernels.evictions).toEqual([
			expect.objectContaining({ lane: "first", reason: "capacity", evicted: true }),
		]);
		expect(created.first[0].isRunning).toBe(false);

		// Every live kernel is running or pinned: no hidden queue, an explicit capacity error.
		kernels.pin("second", "instance:reviewer");
		const lease = await kernels.acquire("third");
		const blocked = kernels.acquire("fourth");
		await expect(blocked).rejects.toBeInstanceOf(KernelPoolCapacityError);
		await expect(blocked).rejects.toMatchObject({ code: "kernel_pool_capacity", live: 2, maxLive: 2 });
		expect(
			kernels
				.stats()
				.lanes.map((lane) => lane.lane)
				.sort(),
		).toEqual(["second", "third"]);
		lease.release();
		expect(() => new KernelPool({ maxLive: 1, idleTtlMs: 1, maxPinned: 2, create: () => created.second[0] })).toThrow(
			"maxPinned",
		);
	});

	test("snapshot-before-evict records non-restorable names and a recreated kernel restores the rest", async () => {
		const directory = scratch();
		const { pool: kernels, created } = pool({ maxLive: 2, snapshotPath: (lane) => join(directory, `${lane}.snap`) });
		await kernels.use("worker", (kernel) => kernel.execute("total = 41\nhelper = lambda x: x + 1"));
		const record = await kernels.evict("worker");
		expect(record).toMatchObject({
			evicted: true,
			restorable: "partial",
			nonRestorable: ["helper"],
			snapshotPath: join(directory, "worker.snap"),
		});
		expect(created.worker[0].isRunning).toBe(false);

		const restored = await kernels.use("worker", (kernel) => kernel.execute("(total + 1, 'helper' in globals())"));
		expect(restored).toMatchObject({ status: "ok", result: "(42, False)" });
		expect(created.worker).toHaveLength(2);
	});

	test("a failed snapshot refuses eviction and keeps the live kernel and its state", async () => {
		const { pool: kernels } = pool({ maxLive: 1, idleTtlMs: 0, snapshotPath: () => "/dev/null/cannot/write.snap" });
		await kernels.use("only", (kernel) => kernel.execute("value = 'precious'"));
		expect(await kernels.sweep()).toEqual([
			expect.objectContaining({
				lane: "only",
				evicted: false,
				refused: "snapshot-failed",
				error: expect.any(String),
			}),
		]);
		// Capacity cannot evict it either, so the new lane gets a capacity error, not state loss.
		await expect(kernels.acquire("other")).rejects.toBeInstanceOf(KernelPoolCapacityError);
		expect(await kernels.use("only", (kernel) => kernel.execute("value"))).toMatchObject({ result: "'precious'" });
	});
});

describe("A43 artifact retention, deletion policy, and quota", () => {
	test("a pinned checkpoint artifact survives delete and reclaim; unpinned artifacts are reclaimed explicitly", async () => {
		const directory = scratch();
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, () => null);
		cleanups.push(() => kernel.shutdown());
		await kernel.execute("state['step'] = 7");
		const snapshotPath = join(directory, "checkpoint.snap");
		expect((await kernel.snapshot(snapshotPath)).status).toBe("ok");

		const documents = new MemoryDocuments();
		const services = new NativeLocalServices(documents);
		const checkpoint = (await services.handle(
			"artifacts.put",
			{ text: readFileSync(snapshotPath, "utf8"), options: { label: "checkpoint", mediaType: "application/json" } },
			context,
		)) as Put;
		const scratchValue = (await services.handle("artifacts.put", { text: "x".repeat(500) }, context)) as Put;
		const evidence = (await services.handle("artifacts.put", { text: "test log: 12 passed" }, context)) as Put;
		expect(
			await services.handle("artifacts.pin", { id: checkpoint.id, holder: "checkpoint:worker@1" }, context),
		).toEqual({ id: checkpoint.id, holders: ["checkpoint:worker@1"] });
		await services.handle("artifacts.pin", { id: evidence.id, holder: "evidence:task-9" }, context);

		await expect(services.handle("artifacts.delete", { id: checkpoint.id }, context)).rejects.toThrow(
			"retained by checkpoint:worker@1",
		);
		const reclaimed = await services.handle("artifacts.reclaim", { bytes: 1_000_000 }, context);
		expect(reclaimed).toEqual({
			deleted: [scratchValue.id],
			freedBytes: 500,
			satisfied: false,
			retained: [checkpoint.id, evidence.id],
		});
		// The deleted artifact is explicitly invalidated, never a dangling or silently empty handle.
		await expect(services.handle("artifacts.read", { id: scratchValue.id }, context)).rejects.toThrow(
			`Artifact ${scratchValue.id} was deleted (reclaimed)`,
		);
		expect(((await services.handle("artifacts.list", {}, context)) as Put[]).map((item) => item.id).sort()).toEqual(
			[checkpoint.id, evidence.id].sort(),
		);

		// Retention survives a service restart; the checkpoint still restores exactly.
		const reopened = new NativeLocalServices(documents);
		await expect(reopened.handle("artifacts.delete", { id: evidence.id }, context)).rejects.toThrow(
			"retained by evidence:task-9",
		);
		const stored = (await reopened.handle(
			"artifacts.read",
			{ id: checkpoint.id, options: { length: 1_048_576 } },
			context,
		)) as { text: string };
		expect(stored.text).toBe(readFileSync(snapshotPath, "utf8"));

		// Releasing the last holder allows deletion.
		await reopened.handle("artifacts.unpin", { id: checkpoint.id, holder: "checkpoint:worker@1" }, context);
		expect(
			await reopened.handle("artifacts.delete", { id: checkpoint.id, reason: "checkpoint superseded" }, context),
		).toEqual({
			id: checkpoint.id,
			deleted: true,
		});
		await expect(reopened.handle("artifacts.read", { id: checkpoint.id }, context)).rejects.toThrow(
			"was deleted (checkpoint superseded)",
		);
		await expect(reopened.handle("artifacts.pin", { id: checkpoint.id, holder: "late" }, context)).rejects.toThrow(
			"was deleted",
		);
	});

	test("quota failures are explicit and never evict artifacts to make room", async () => {
		const services = new NativeLocalServices(new MemoryDocuments(), { artifactQuotaBytes: 1_000 });
		const first = (await services.handle("artifacts.put", { text: "a".repeat(600) }, context)) as Put;
		await services.handle("artifacts.pin", { id: first.id, holder: "task:live" }, context);
		const second = (await services.handle("artifacts.put", { text: "b".repeat(300) }, context)) as Put;

		const rejected = services.handle("artifacts.put", { text: "c".repeat(200) }, context);
		await expect(rejected).rejects.toBeInstanceOf(ArtifactQuotaError);
		await expect(rejected).rejects.toMatchObject({
			code: "artifact_quota_exceeded",
			usedBytes: 900,
			requestedBytes: 200,
			quotaBytes: 1_000,
		});
		// Nothing was removed to make room, and the rejected content was not stored.
		expect(await services.handle("artifacts.usage", {}, context)).toEqual({
			usedBytes: 900,
			quotaBytes: 1_000,
			count: 2,
			pinned: [first.id],
		});
		// Re-putting existing content deduplicates and needs no quota.
		expect(await services.handle("artifacts.put", { text: "a".repeat(600) }, context)).toMatchObject({
			id: first.id,
		});

		// Explicit reclaim frees only unpinned bytes; then the put succeeds.
		expect(await services.handle("artifacts.reclaim", { bytes: 200 }, context)).toMatchObject({
			deleted: [second.id],
			freedBytes: 300,
			satisfied: true,
			retained: [first.id],
		});
		expect(await services.handle("artifacts.put", { text: "c".repeat(200) }, context)).toMatchObject({ bytes: 200 });
		expect(await services.handle("artifacts.usage", {}, context)).toMatchObject({ usedBytes: 800, count: 2 });
		// A single artifact larger than the whole quota is rejected outright.
		await expect(services.handle("artifacts.put", { text: "d".repeat(1_001) }, context)).rejects.toThrow(
			"Artifact quota exceeded",
		);
	});
});
