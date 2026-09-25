import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import {
	DEFAULT_KERNEL_LIMITS,
	type KernelResourceLimits,
	kernelResourceLimits,
	RlmKernel,
} from "../src/ultron/rlm/kernel.ts";

// Memory and CPU limits for Python kernels: resource limits, not isolation.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const kernels: RlmKernel[] = [];
const MiB = 1024 * 1024;

afterEach(async () => {
	await Promise.all(kernels.splice(0).map((kernel) => kernel.shutdown()));
});

function kernel(limits?: Partial<KernelResourceLimits>): RlmKernel {
	const instance = new RlmKernel({ cwd: process.cwd(), runtimePath, limits }, () => null);
	kernels.push(instance);
	return instance;
}

describe.skipIf(process.platform !== "linux")("RLM kernel resource limits", () => {
	test("defaults are generous, configurable by environment, and 0 disables a limit", () => {
		expect(DEFAULT_KERNEL_LIMITS).toEqual({ maxMemoryMb: 4096, maxCpuSeconds: 1800 });
		expect(kernelResourceLimits({})).toEqual({ maxMemoryMb: 4096, maxCpuSeconds: 1800 });
		expect(kernelResourceLimits({ ULTRON_RLM_MAX_MEMORY_MB: "512", ULTRON_RLM_MAX_CPU_SECONDS: "0" })).toEqual({
			maxMemoryMb: 512,
			maxCpuSeconds: 0,
		});
		// Malformed values fall back to the defaults instead of silently disabling a limit.
		expect(kernelResourceLimits({ ULTRON_RLM_MAX_MEMORY_MB: "lots", ULTRON_RLM_MAX_CPU_SECONDS: "-1" })).toEqual(
			DEFAULT_KERNEL_LIMITS,
		);
		expect(kernelResourceLimits({ ULTRON_RLM_MAX_MEMORY_MB: "512" }, { maxMemoryMb: 64 }).maxMemoryMb).toBe(64);
		expect(() => kernelResourceLimits({}, { maxCpuSeconds: 1.5 })).toThrow(/non-negative integer/);
	});

	test("default limits are applied and do not affect normal cells", async () => {
		const instance = kernel();
		const result = await instance.execute(
			[
				"import resource",
				"data = bytearray(200 * 1024 * 1024)",
				"sum(range(2_000_000))",
				"(resource.getrlimit(resource.RLIMIT_DATA), resource.getrlimit(resource.RLIMIT_CPU)[0] > 0, len(data))",
			].join("\n"),
		);
		expect(result).toMatchObject({ status: "ok", result: `((${4096 * MiB}, ${4096 * MiB}), True, ${200 * MiB})` });

		const unlimited = kernel({ maxMemoryMb: 0, maxCpuSeconds: 0 });
		expect(
			await unlimited.execute(
				"import resource\nresource.getrlimit(resource.RLIMIT_DATA)[0] != 4096 * 1024 * 1024 and resource.getrlimit(resource.RLIMIT_CPU)[0] == resource.RLIM_INFINITY",
			),
		).toMatchObject({ status: "ok", result: "True" });
	});

	test("a cell allocating beyond the memory limit fails clearly and the next cell works", async () => {
		const instance = kernel({ maxMemoryMb: 256, maxCpuSeconds: 0 });
		expect(await instance.execute("kept = 'still here'")).toMatchObject({ status: "ok" });
		const failed = await instance.execute("hog = bytearray(512 * 1024 * 1024)");
		expect(failed.status).toBe("error");
		expect(failed.error?.ename).toBe("MemoryError");
		expect(failed.error?.evalue).toContain("RLM kernel exceeded its memory limit (256 MiB per process");
		// The allocation failed inside Python, so the kernel and its state survive.
		expect(await instance.execute("(kept, 'hog' in globals(), 6 * 7)")).toMatchObject({
			status: "ok",
			result: "('still here', False, 42)",
		});
		// Cell code cannot raise the hard limit back up for itself.
		const raised = await instance.execute(
			"import resource\nresource.setrlimit(resource.RLIMIT_DATA, (resource.RLIM_INFINITY, resource.RLIM_INFINITY))",
		);
		expect(raised.status).toBe("error");
	}, 30_000);

	test("a kernel that dies of its memory limit is reported and replaced for the next cell", async () => {
		const instance = kernel({ maxMemoryMb: 256 });
		expect(await instance.execute("before = 1")).toMatchObject({ status: "ok" });
		// runtime.py exits with this code when it cannot even report a MemoryError in-band.
		await expect(instance.execute("import os\nos._exit(86)")).rejects.toThrow(
			/RLM kernel exceeded its memory limit \(256 MiB per process.*fresh kernel/,
		);
		expect(await instance.execute("('before' in globals(), 1 + 1)")).toMatchObject({
			status: "ok",
			result: "(False, 2)",
		});
	}, 30_000);

	test("a CPU-burning cell is stopped by the per-cell CPU limit and the kernel keeps its state", async () => {
		const instance = kernel({ maxCpuSeconds: 1 });
		expect(await instance.execute("kept = 5")).toMatchObject({ status: "ok" });
		const burned = await instance.execute("while True:\n    pass");
		expect(burned.status).toBe("error");
		expect(burned.error?.ename).toBe("RlmCpuLimitExceeded");
		expect(burned.error?.evalue).toContain("exceeded its CPU limit of 1 CPU-seconds");
		// The budget is per cell: the next cell gets a fresh one.
		expect(await instance.execute("sum(range(100_000)) and kept")).toMatchObject({ status: "ok", result: "5" });
	}, 30_000);

	test("a cell that swallows the CPU error or is stuck in C code is killed by the host backstop", async () => {
		const instance = kernel({ maxCpuSeconds: 1 });
		const swallow =
			"while True:\n    try:\n        while True:\n            pass\n    except BaseException:\n        pass";
		await expect(instance.execute(swallow)).rejects.toThrow(/exceeded its CPU limit \(1 CPU-seconds per cell/);
		expect(await instance.execute("1 + 1")).toMatchObject({ status: "ok", result: "2" });
		// sum() over a range loops in C without checking for signals, so the Python handler cannot run.
		await expect(instance.execute("sum(range(10 ** 12))")).rejects.toThrow(/exceeded its CPU limit/);
		expect(await instance.execute("2 + 2")).toMatchObject({ status: "ok", result: "4" });
	}, 30_000);

	test("limits propagate to subprocesses a cell spawns", async () => {
		const instance = kernel({ maxMemoryMb: 256, maxCpuSeconds: 2 });
		const probe = await instance.execute(
			[
				"import subprocess, sys",
				"code = 'import resource; print(resource.getrlimit(resource.RLIMIT_DATA)[0], resource.getrlimit(resource.RLIMIT_CPU)[0]); bytearray(512 * 1024 * 1024)'",
				"child = subprocess.run([sys.executable, '-c', code], capture_output=True, text=True)",
				"burner = subprocess.run([sys.executable, '-c', 'while True: pass'])",
				"data, cpu = map(int, child.stdout.split())",
				"(data, 0 < cpu < 60, child.returncode != 0, 'MemoryError' in child.stderr, burner.returncode)",
			].join("\n"),
		);
		// SIGXCPU (24) terminates the CPU-burning child; the kernel's own budget is unaffected.
		expect(probe).toMatchObject({ status: "ok", result: `(${256 * MiB}, True, True, True, -24)` });
	}, 30_000);
});
