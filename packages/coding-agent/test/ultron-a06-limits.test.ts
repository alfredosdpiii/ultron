/**
 * A06: output, artifact, wall-time, concurrency, and process limits hold.
 *
 * Profile under test (trusted-local, the only profile the native worker supports today):
 * - Output: 8 KiB preview per stdout/stderr/result stream (runtime.py); protocol output is capped
 *   at 1 MiB per frame (runtime.py shrinks its own frames to fit) and 4 MiB per cell stream
 *   (kernel.ts), never cumulatively over a kernel's lifetime (ultron-kernel-long-session.test.ts).
 * - Artifacts: maxArtifactBytes (default 256 MiB) and maxArtifactStorageBytes (default 1 GiB)
 *   in NativeLocalServices; from Python, a host request over the 1 MiB frame raises in the cell.
 * - Wall time: per-task timeout_ms in NativeRlmHost and root maxWallMs in the usage ledger;
 *   a cell is interrupted through its AbortSignal.
 * - Concurrency: maxAdmittedTasks: 24 unfinished task reservations per root (worker profile).
 * - Process: the kernel owns a process group; interrupt and shutdown SIGKILL the whole group.
 */
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Context, JsonValue } from "@ultron/chord";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { type DurableDocumentStorage, NativeLocalServices } from "../src/ultron/local-services.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const context = {} as Context;
const PREVIEW_LIMIT = 8192 + Buffer.byteLength("\n... [truncated]");
/** Captured stdout/stderr keep their head and tail within the output budget, plus the middle marker. */
const OUTPUT_LIMIT = 20_000 + 64;
const WORKER_MAX_ADMITTED_TASKS = 24;

function memoryStore(): NativeHostStore & { value: JsonValue | undefined } {
	return {
		value: undefined,
		async read() {
			return structuredClone(this.value) as never;
		},
		async write(next) {
			this.value = structuredClone(next) as JsonValue;
		},
	};
}

/** A lane whose prompt never settles, so admitted tasks stay unfinished until cancelled. */
function hangingLane() {
	return {
		findEntries: async () => [],
		getActiveTools: async () => [],
		setActiveTools: async () => {},
		setModel: async () => {},
		prompt: () => new Promise<never>(() => {}),
		abort: async () => ({ ok: true }),
	};
}

function nativeHost(store: NativeHostStore, usage?: NativeUsageLedger) {
	const lane = hangingLane();
	return new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store, usage });
}

function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
	} catch {
		return false;
	}
	// A killed child can linger as a zombie until its new parent reaps it.
	try {
		const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
		return stat.slice(stat.lastIndexOf(")") + 2)[0] !== "Z";
	} catch {
		return false;
	}
}

async function waitFor(predicate: () => boolean, timeoutMs = 5000): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	while (!predicate()) {
		if (Date.now() > deadline) throw new Error("Timed out waiting for condition");
		await new Promise((resolve) => setTimeout(resolve, 25));
	}
}

async function readPids(path: string): Promise<number[]> {
	let pids: number[] = [];
	await waitFor(() => {
		try {
			pids = readFileSync(path, "utf8").trim().split(/\s+/).map(Number);
			return pids.length === 4 && pids.every((pid) => Number.isSafeInteger(pid) && pid > 0);
		} catch {
			return false;
		}
	});
	return pids;
}

/** Kernel pid, a direct `sleep` child, a `sh` child, and the `sleep` grandchild it started. */
function spawnTreeCell(pidFile: string): string {
	return [
		"import os, subprocess, time",
		"child = subprocess.Popen(['sleep', '60'])",
		"shell = subprocess.Popen(['sh', '-c', 'sleep 60 & echo $!; wait'], stdout=subprocess.PIPE, text=True)",
		"grandchild = int(shell.stdout.readline())",
		`with open(${JSON.stringify(pidFile)}, 'w') as f: f.write(f'{os.getpid()} {child.pid} {shell.pid} {grandchild}')`,
		"time.sleep(60)",
	].join("\n");
}

describe("A06 limits", () => {
	let dir: string;
	beforeAll(() => {
		dir = mkdtempSync(join(tmpdir(), "ultron-a06-"));
	});
	afterAll(() => {
		rmSync(dir, { recursive: true, force: true });
	});

	describe("output flooding", () => {
		test("stdout, stderr, and result previews stay bounded under a 50 MB flood", async () => {
			const kernel = new RlmKernel({ cwd: dir, runtimePath }, async () => null);
			try {
				const result = await kernel.execute(
					[
						"import sys",
						"for _ in range(5000): print('o' * 10000)",
						"for _ in range(5000): print('e' * 10000, file=sys.stderr)",
						"['r' * 10000 for _ in range(5000)]",
					].join("\n"),
				);
				expect(result.status).toBe("ok");
				for (const stream of [result.stdout, result.stderr]) {
					expect(Buffer.byteLength(stream)).toBeLessThanOrEqual(OUTPUT_LIMIT);
					expect(stream).toMatch(/\[\.\.\. \d+ bytes truncated \.\.\.\]/);
				}
				// The 50 MB list is shown by reference: its size and bounded head and tail.
				expect(Buffer.byteLength(result.result ?? "")).toBeLessThanOrEqual(PREVIEW_LIMIT);
				expect(result.result).toContain("<list: 5,000 items; item types str>");
				// A flooding exception message and traceback are bounded too.
				const failed = await kernel.execute("raise ValueError('x' * 5_000_000)");
				expect(failed.status).toBe("error");
				expect(Buffer.byteLength(failed.error!.evalue)).toBeLessThanOrEqual(PREVIEW_LIMIT);
				expect(Buffer.byteLength(failed.error!.traceback.join("\n"))).toBeLessThanOrEqual(PREVIEW_LIMIT);
			} finally {
				await kernel.shutdown();
			}
		}, 30_000);

		test.skipIf(process.platform === "win32")(
			"raw writes that bypass Python's stdout are bounded output, not protocol",
			async () => {
				const kernel = new RlmKernel({ cwd: dir, runtimePath }, async () => null);
				try {
					const pid = (await kernel.execute("import os\nkept = 'intact'\nos.getpid()")).result;
					// The protocol runs on fd 3, so 8 MiB on fd 1 is the cell's output, cut to the per-cell limit.
					const result = await kernel.execute("os.write(1, b'y' * (8 * 1024 * 1024))\n'after'");
					expect(result).toMatchObject({ status: "ok", result: "'after'" });
					expect(result.stdout.length).toBeLessThanOrEqual(4 * 1024 * 1024 + 256);
					expect(result.stdout).toMatch(/over the RLM kernel's 4 MiB per-cell output limit/);
					// The same kernel process keeps its variables.
					expect(await kernel.execute("os.getpid(), kept")).toMatchObject({
						status: "ok",
						result: `(${pid}, 'intact')`,
					});
				} finally {
					await kernel.shutdown();
				}
			},
			30_000,
		);
	});

	describe("artifact size", () => {
		function documents(): DurableDocumentStorage {
			const values = new Map<string, JsonValue>();
			return {
				get: async (key) => structuredClone(values.get(key)),
				set: async (key, value) => {
					values.set(key, structuredClone(value));
				},
				list: async (prefix) =>
					[...values.entries()]
						.filter(([key]) => key.startsWith(prefix))
						.map(([key, value]) => ({ key, value: structuredClone(value) })),
			};
		}

		test("oversize artifacts and quota overflow are rejected without being stored", async () => {
			const services = new NativeLocalServices(documents(), {
				maxArtifactBytes: 1024,
				maxArtifactStorageBytes: 1500,
			});
			await expect(services.handle("artifacts.put", { text: "a".repeat(1025) }, context)).rejects.toThrow(
				"exceeds the 1024-byte artifact limit",
			);
			// Multi-byte text is measured in UTF-8 bytes, not characters.
			await expect(services.handle("artifacts.put", { text: "é".repeat(513) }, context)).rejects.toThrow(
				"artifact limit",
			);
			const first = (await services.handle("artifacts.put", { text: "b".repeat(1000) }, context)) as {
				id: string;
			};
			// Storing identical content again deduplicates and does not consume quota.
			await expect(services.handle("artifacts.put", { text: "b".repeat(1000) }, context)).resolves.toMatchObject({
				id: first.id,
			});
			await expect(services.handle("artifacts.put", { text: "c".repeat(600) }, context)).rejects.toThrow(
				"storage quota of 1500 bytes exceeded",
			);
			expect(await services.handle("artifacts.list", {}, context)).toEqual([
				expect.objectContaining({ id: first.id, bytes: 1000 }),
			]);
			expect(() => new NativeLocalServices(documents(), { maxArtifactBytes: 0 })).toThrow(
				"Artifact limits must be positive integers",
			);
		});

		test("an artifact larger than the RPC frame never reaches the service from Python", async () => {
			const requests: string[] = [];
			const kernel = new RlmKernel({ cwd: dir, runtimePath }, async (type) => {
				requests.push(type);
				return { id: "stored" };
			});
			try {
				await kernel.execute("kept = 1");
				// Refused in the cell (a ValueError), so the kernel and its variables survive.
				const refused = await kernel.execute(
					"await rlm.host_request('artifacts.put', {'text': 'z' * (2 * 1024 * 1024)})",
				);
				expect(refused).toMatchObject({ status: "error", error: { ename: "ValueError" } });
				expect(refused.error?.evalue).toContain("over the kernel's 1 MiB frame");
				expect(requests).toEqual([]);
				expect(await kernel.execute("kept")).toMatchObject({ result: "1" });
				expect(await kernel.execute("await rlm.host_request('artifacts.put', {'text': 'small'})")).toMatchObject({
					result: "{'id': 'stored'}",
				});
				expect(requests).toEqual(["artifacts.put"]);
			} finally {
				await kernel.shutdown();
			}
		}, 30_000);
	});

	describe("wall time", () => {
		test("a task past its timeout is cancelled with a durable cancelled result", async () => {
			const store = memoryStore();
			const host = nativeHost(store);
			const spawned = (await host.handle(
				"agents.spawn",
				{ definition: "security-reviewer@1", input: { request: "review" }, timeout_ms: 50 },
				context,
			)) as { id: string };
			await expect(host.handle("agents.result", { id: spawned.id }, context)).resolves.toMatchObject({
				status: "cancelled",
				error: "Ultron task exceeded 50ms timeout",
			});
			await host.close();
			// A new host over the same journal sees the committed terminal result.
			const reopened = nativeHost(store);
			expect(await reopened.handle("agents.inspect", { id: spawned.id }, context)).toMatchObject({
				state: "cancelled",
				result: { status: "cancelled", error: "Ultron task exceeded 50ms timeout" },
			});
			await reopened.close();
		});

		test("the root wall deadline caps task timeouts and rejects work after it passes", async () => {
			const host = nativeHost(memoryStore(), new NativeUsageLedger(undefined, { limits: { maxWallMs: 200 } }));
			const spawned = (await host.handle(
				"agents.spawn",
				{ definition: "security-reviewer@1", input: { request: "review" }, timeout_ms: 100 },
				context,
			)) as { id: string };
			await expect(host.handle("agents.result", { id: spawned.id }, context)).resolves.toMatchObject({
				status: "cancelled",
			});
			await new Promise((resolve) => setTimeout(resolve, 200));
			await expect(
				host.handle(
					"agents.spawn",
					{ definition: "security-reviewer@1", input: { request: "late" }, timeout_ms: 100 },
					context,
				),
			).rejects.toThrow("Usage wall deadline exceeded");
			await host.close();
		});

		test("an interrupted runaway cell stops, and the kernel serves the next cell", async () => {
			const kernel = new RlmKernel({ cwd: dir, runtimePath }, async () => null);
			try {
				await kernel.execute("marker = 1");
				const started = Date.now();
				await expect(kernel.execute("while True:\n    pass", AbortSignal.timeout(300))).rejects.toThrow();
				expect(Date.now() - started).toBeLessThan(5000);
				// Interrupt replaces the process; state is lost rather than silently restored.
				expect(await kernel.execute("'marker' in globals()")).toMatchObject({ result: "False" });
			} finally {
				await kernel.shutdown();
			}
		}, 30_000);
	});

	describe("concurrency and admission", () => {
		test(`admission beyond ${WORKER_MAX_ADMITTED_TASKS} unfinished tasks is rejected until one finishes`, async () => {
			const usage = new NativeUsageLedger(undefined, {
				limits: { maxAdmittedTasks: WORKER_MAX_ADMITTED_TASKS, maxWallMs: 30 * 60 * 1000 },
			});
			const store = memoryStore();
			const host = nativeHost(store, usage);
			const request = (index: number) => ({
				definition: "security-reviewer@1",
				input: { request: `review ${index}` },
				timeout_ms: 60_000,
			});
			const ids: string[] = [];
			for (let index = 0; index < WORKER_MAX_ADMITTED_TASKS; index += 1) {
				ids.push(((await host.handle("agents.spawn", request(index), context)) as { id: string }).id);
			}
			await expect(host.handle("agents.spawn", request(99), context)).rejects.toThrow(
				"Usage admitted-task limit exceeded",
			);
			// The rejected request was never journaled, so nothing is hidden in a queue.
			const status = (await host.handle("agents.status", {}, context)) as {
				tasks: unknown[];
				limits: { maxAdmittedTasks: number };
			};
			expect(status.tasks).toHaveLength(WORKER_MAX_ADMITTED_TASKS);
			expect(status.limits.maxAdmittedTasks).toBe(WORKER_MAX_ADMITTED_TASKS);

			await expect(host.handle("agents.cancel", { id: ids[0] }, context)).resolves.toEqual({ cancelled: true });
			await expect(host.handle("agents.spawn", request(100), context)).resolves.toMatchObject({
				state: expect.any(String),
			});
			await expect(host.handle("agents.spawn", request(101), context)).rejects.toThrow(
				"Usage admitted-task limit exceeded",
			);
			await host.close();
		});
	});

	describe("process tree", () => {
		test("interrupting a cell kills its child and grandchild processes", async () => {
			const pidFile = join(dir, "interrupt.pids");
			const kernel = new RlmKernel({ cwd: dir, runtimePath }, async () => null);
			const controller = new AbortController();
			try {
				const running = kernel.execute(spawnTreeCell(pidFile), controller.signal);
				const pids = await readPids(pidFile);
				expect(pids.every(alive)).toBe(true);
				controller.abort(new Error("interrupt"));
				await expect(running).rejects.toThrow("interrupt");
				await waitFor(() => !pids.some(alive));
			} finally {
				await kernel.shutdown();
			}
		}, 30_000);

		test("shutting down the kernel kills the whole process tree", async () => {
			const pidFile = join(dir, "shutdown.pids");
			const kernel = new RlmKernel({ cwd: dir, runtimePath }, async () => null);
			const running = kernel.execute(spawnTreeCell(pidFile));
			running.catch(() => {});
			const pids = await readPids(pidFile);
			expect(pids.every(alive)).toBe(true);
			await kernel.shutdown();
			await expect(running).rejects.toThrow("RLM kernel is shut down");
			await waitFor(() => !pids.some(alive));
		}, 30_000);
	});
});
