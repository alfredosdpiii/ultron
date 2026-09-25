import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { createInstanceModule } from "../src/ultron/instances.ts";
import { createMemoryModuleStore } from "../src/ultron/rlm/host-module.ts";
import { KernelPool, type PoolableKernel } from "../src/ultron/rlm/kernel-pool.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { memoryDefinitionStore, memoryStore, scriptedHarness } from "./ultron-host-fixtures.ts";

/** Retained instances pin their lane's kernel so an idle retained agent keeps its Python process. */

function fakePool(options: { maxLive: number; maxPinned: number }) {
	let now = 0;
	const pool = new KernelPool<PoolableKernel>({
		create: () => ({ snapshot: async () => ({ status: "ok" }) as never, shutdown: async () => {} }),
		maxLive: options.maxLive,
		maxPinned: options.maxPinned,
		idleTtlMs: 1_000,
		now: () => now,
	});
	const pin = (lane: string, holder: string): boolean => {
		try {
			pool.pin(lane, holder);
			return true;
		} catch {
			return false;
		}
	};
	return {
		pool,
		pin,
		advance: (ms: number) => {
			now += ms;
		},
		touch: (lane: string) => pool.use(lane, async () => {}),
	};
}

async function retainedHost(
	pool: ReturnType<typeof fakePool>,
	stores = { tasks: memoryStore(), instances: createMemoryModuleStore() },
) {
	const fake = scriptedHarness(() => "done");
	const host = new NativeRlmHost(fake.harness as never, {} as never, {
		store: stores.tasks,
		definitionStore: memoryDefinitionStore(),
		modules: [createInstanceModule({ store: stores.instances })],
		pinLane: pool.pin,
		unpinLane: (lane, holder) => pool.pool.unpin(lane, holder),
	});
	const call = <T>(type: string, payload: Record<string, unknown> = {}) =>
		host.handle(type, payload, {} as never) as Promise<T>;
	const retain = async () => {
		const task = await call<{ id: string }>("agents.spawn", { definition: "rlm-child@1", input: { prompt: "x" } });
		await call("agents.result", { id: task.id });
		return call<{ id: string; lane: string; pinned: boolean }>("instances.retain", { task_id: task.id });
	};
	return { host, call, retain, stores };
}

describe("retained instances pin their kernel", () => {
	test("a pinned instance lane survives capacity pressure and idle sweeps; closing makes it evictable", async () => {
		const pool = fakePool({ maxLive: 2, maxPinned: 1 });
		const { host, call, retain } = await retainedHost(pool);
		const instance = await retain();
		expect(instance.pinned).toBe(true);
		await pool.touch(instance.lane);
		pool.advance(10);
		await pool.touch("other-1");
		pool.advance(10);
		// Capacity pressure evicts the idle unpinned lane, not the older pinned one.
		await pool.touch("other-2");
		expect(pool.pool.live(instance.lane)).toBeDefined();
		expect(pool.pool.live("other-1")).toBeUndefined();
		pool.advance(5_000);
		await pool.pool.sweep();
		expect(pool.pool.live(instance.lane)).toBeDefined();
		expect(pool.pool.live("other-2")).toBeUndefined();

		await call("instances.close", { id: instance.id });
		await pool.pool.sweep();
		expect(pool.pool.live(instance.lane)).toBeUndefined();
		await host.close();
	});

	test("retain still succeeds with pinned: false once pin capacity is spent", async () => {
		const pool = fakePool({ maxLive: 4, maxPinned: 1 });
		const { host, call, retain } = await retainedHost(pool);
		expect((await retain()).pinned).toBe(true);
		const second = await retain();
		expect(second).toMatchObject({ pinned: false, state: "open" });
		await expect(call("instances.get", { id: second.id })).resolves.toMatchObject({ state: "open" });
		await host.close();
	});

	test("a restarted owner re-pins its open instances, not closed ones", async () => {
		const first = fakePool({ maxLive: 4, maxPinned: 4 });
		const owner = await retainedHost(first);
		const open = await owner.retain();
		const closed = await owner.retain();
		await owner.call("instances.close", { id: closed.id });
		await owner.host.close();

		const pool = fakePool({ maxLive: 4, maxPinned: 4 });
		const restarted = await retainedHost(pool, owner.stores);
		await restarted.call("ping");
		await pool.touch(open.lane);
		await pool.touch(closed.lane);
		expect((await pool.pool.evict(open.lane)).refused).toBe("pinned");
		expect((await pool.pool.evict(closed.lane)).evicted).toBe(true);
		await restarted.host.close();
	});
});

describe("rlm tool pins", () => {
	test("pins are bounded to half the pool so new work keeps room", async () => {
		const dir = mkdtempSync(join(tmpdir(), "ultron-pin-"));
		const tool = createUltronRlmTool(dir, async () => null, undefined, { maxLive: 4 });
		try {
			expect(tool.pin("a", "instance:1")).toBe(true);
			expect(tool.pin("a", "instance:2")).toBe(true);
			expect(tool.pin("b", "instance:3")).toBe(true);
			expect(tool.pin("c", "instance:4")).toBe(false);
			tool.unpin("a", "instance:1");
			expect(tool.pin("c", "instance:4")).toBe(false);
			tool.unpin("a", "instance:2");
			expect(tool.pin("c", "instance:4")).toBe(true);
		} finally {
			await tool.close();
			rmSync(dir, { recursive: true, force: true });
		}
	});
});
