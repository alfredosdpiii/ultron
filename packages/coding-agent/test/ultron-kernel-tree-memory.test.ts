import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { cgroupScopeAvailable, kernelTreeMemoryLimit, treeMemoryBackend } from "../src/ultron/rlm/tree-memory.ts";

// Whole-tree memory cap: the kernel plus every process its cells start, on top of the per-process RLIMIT_DATA.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const kernels: RlmKernel[] = [];

afterEach(async () => {
	await Promise.all(kernels.splice(0).map((kernel) => kernel.shutdown()));
});

function kernel(backend: "cgroup" | "watchdog", maxTreeMemoryMb: number): RlmKernel {
	const instance = new RlmKernel(
		{
			cwd: process.cwd(),
			runtimePath,
			limits: { maxMemoryMb: 256, maxCpuSeconds: 0 },
			maxTreeMemoryMb,
			treeMemoryBackend: backend,
		},
		() => null,
	);
	kernels.push(instance);
	return instance;
}

/** Starts `count` subprocesses that each touch `mb` MiB (under the per-process limit) and then wait. */
function hogs(count: number, mb: number): string {
	return [
		"import subprocess, sys, time",
		`code = "import time\\nb = b'x' * (${mb} * 1024 * 1024)\\ntime.sleep(60)"`,
		`procs = [subprocess.Popen([sys.executable, '-c', code]) for _ in range(${count})]`,
		"for p in procs: p.wait()",
		"'finished'",
	].join("\n");
}

describe("tree memory limit configuration", () => {
	test("defaults to twice the per-process limit; the environment and overrides set it; 0 disables", () => {
		expect(kernelTreeMemoryLimit({}, 4096, undefined)).toBe(8192);
		expect(kernelTreeMemoryLimit({ ULTRON_RLM_MAX_TREE_MEMORY_MB: "1000" }, 4096, undefined)).toBe(1000);
		expect(kernelTreeMemoryLimit({ ULTRON_RLM_MAX_TREE_MEMORY_MB: "lots" }, 512, undefined)).toBe(1024);
		expect(kernelTreeMemoryLimit({ ULTRON_RLM_MAX_TREE_MEMORY_MB: "1000" }, 4096, 64)).toBe(64);
		expect(kernelTreeMemoryLimit({}, 0, undefined)).toBe(0);
		expect(treeMemoryBackend(0, "cgroup")).toBe("off");
		expect(treeMemoryBackend(100, "off")).toBe("off");
		if (process.platform === "linux") expect(treeMemoryBackend(100, "watchdog")).toBe("watchdog");
	});
});

const backends = [
	["watchdog", process.platform === "linux"],
	["cgroup", process.platform === "linux" && cgroupScopeAvailable()],
] as const;

for (const [backend, available] of backends) {
	describe.skipIf(!available)(`RLM kernel tree memory limit (${backend})`, () => {
		test("subprocesses that together exceed the tree cap are stopped with a clear error; the next cell works", async () => {
			const instance = kernel(backend, 320);
			expect(await instance.execute("kept = 1")).toMatchObject({ status: "ok" });
			// Each child stays under the 256 MiB per-process limit; four of them exceed 320 MiB together.
			const started = Date.now();
			const outcome = await instance.execute(hogs(4, 150)).then(
				(result) => new Error(`cell was not stopped: ${JSON.stringify(result)}`),
				(error: Error) => error,
			);
			expect(outcome.message).toMatch(
				/RLM kernel process tree exceeded its memory limit \(320 MiB total .*ULTRON_RLM_MAX_TREE_MEMORY_MB.*fresh kernel/,
			);
			expect(Date.now() - started).toBeLessThan(20_000);
			// The whole tree is gone and a fresh kernel serves the next cell.
			expect(await instance.execute("('kept' in globals(), 6 * 7)")).toMatchObject({
				status: "ok",
				result: "(False, 42)",
			});
		}, 40_000);

		test("normal cells and modest subprocesses are unaffected", async () => {
			const instance = kernel(backend, 512);
			const result = await instance.execute(
				[
					"import subprocess, sys",
					"data = bytearray(100 * 1024 * 1024)",
					"out = subprocess.run([sys.executable, '-c', \"b = b'x' * (100 * 1024 * 1024); print(len(b))\"], capture_output=True, text=True)",
					"(len(data) // 2**20, out.stdout.strip())",
				].join("\n"),
			);
			expect(result).toMatchObject({ status: "ok", result: `(100, '${100 * 1024 * 1024}')` });
			expect(await instance.execute("1 + 1")).toMatchObject({ status: "ok", result: "2" });
		}, 30_000);
	});
}
