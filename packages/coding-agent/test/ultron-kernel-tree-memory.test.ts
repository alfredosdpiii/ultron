import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import {
	cgroupScopeAvailable,
	kernelTreeMemoryLimit,
	processMemoryBytes,
	processStat,
	treeMemoryBackend,
	watchdogPollMs,
} from "../src/ultron/rlm/tree-memory.ts";

// Whole-tree memory cap: the kernel plus every process its cells start, on top of the per-process RLIMIT_DATA.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const kernels: RlmKernel[] = [];
const dirs: string[] = [];
const daemons: number[] = [];

afterEach(async () => {
	await Promise.all(kernels.splice(0).map((kernel) => kernel.shutdown()));
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
	// Only matters when a test fails: never leave an escaped daemon behind.
	for (const pid of daemons.splice(0)) {
		try {
			process.kill(pid, "SIGKILL");
		} catch {
			/* Gone, as expected. */
		}
	}
});

function scratch(): string {
	const dir = mkdtempSync(join(tmpdir(), "ultron-tree-memory-"));
	dirs.push(dir);
	return dir;
}

/**
 * Cell code that starts `code` as a daemon which escapes the cell's process tree the classic way: a launcher
 * forks, the child calls setsid() and forks again, and both intermediate processes exit at once. The daemon
 * writes its pid to `pidFile`.
 */
function doubleFork(pidFile: string, code: string): string {
	const daemon = `import os\nopen(${JSON.stringify(pidFile)}, 'w').write(str(os.getpid()))\n${code}`;
	const launcher = [
		"import os, sys",
		"if os.fork() == 0:",
		"    os.setsid()",
		"    if os.fork() == 0:",
		`        os.execv(sys.executable, [sys.executable, '-c', ${JSON.stringify(daemon)}])`,
		"    os._exit(0)",
		"os.wait()",
	].join("\n");
	return [
		"import subprocess, sys",
		`subprocess.run([sys.executable, '-c', ${JSON.stringify(launcher)}], check=True)`,
	].join("\n");
}

async function daemonPid(pidFile: string): Promise<number> {
	for (let attempt = 0; attempt < 100 && !existsSync(pidFile); attempt++) await new Promise((r) => setTimeout(r, 50));
	const pid = Number(readFileSync(pidFile, "utf8"));
	daemons.push(pid);
	return pid;
}

/** Waits until `pid` has exited (or is a zombie awaiting its reaper); returns whether it did. */
async function gone(pid: number): Promise<boolean> {
	for (let attempt = 0; attempt < 60; attempt++) {
		const stat = processStat(pid);
		if (!stat || stat.state === "Z") return true;
		await new Promise((r) => setTimeout(r, 50));
	}
	return false;
}

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

	test("the watchdog polls faster near the cap and counts proportional memory", () => {
		expect(watchdogPollMs(10, 100)).toBe(500);
		expect(watchdogPollMs(50, 100)).toBe(250);
		expect(watchdogPollMs(80, 100)).toBe(100);
		if (process.platform === "linux") expect(processMemoryBytes(process.pid)).toBeGreaterThan(0);
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

		test("a double-forked memory hog is still counted and killed, and does not outlive the kernel", async () => {
			const instance = kernel(backend, 200);
			const pidFile = join(scratch(), "daemon.pid");
			// 210 MiB stays under the 256 MiB per-process limit but exceeds the 200 MiB tree cap on its own; it is
			// caught only if the escaped daemon is still counted as part of the tree.
			const cell = [
				doubleFork(pidFile, "import time\nb = b'x' * (210 * 1024 * 1024)\ntime.sleep(60)"),
				"import time",
				"time.sleep(30)",
				"'finished'",
			].join("\n");
			const outcome = await instance.execute(cell).then(
				(result) => new Error(`cell was not stopped: ${JSON.stringify(result)}`),
				(error: Error) => error,
			);
			expect(outcome.message).toMatch(/RLM kernel process tree exceeded its memory limit \(200 MiB total/);
			expect(await gone(await daemonPid(pidFile))).toBe(true);
			expect(await instance.execute("1 + 1")).toMatchObject({ status: "ok", result: "2" });
		}, 60_000);

		test("a double-forked daemon is adopted by the kernel and stopped with it", async () => {
			const instance = kernel(backend, 512);
			const pidFile = join(scratch(), "daemon.pid");
			expect(
				await instance.execute(`${doubleFork(pidFile, "import time\ntime.sleep(60)")}\n'started'`),
			).toMatchObject({ status: "ok", result: "'started'" });
			const pid = await daemonPid(pidFile);
			// The kernel is a child subreaper, so the orphaned daemon is its child rather than init's.
			expect(await instance.execute("__import__('os').getpid()")).toMatchObject({
				status: "ok",
				result: String(processStat(pid)?.ppid),
			});
			await instance.shutdown();
			expect(await gone(pid)).toBe(true);
		}, 30_000);

		test("forked children sharing a large copy-on-write buffer count it once", async () => {
			const instance = kernel(backend, 320);
			// One 150 MiB buffer shared by a parent and four forked children: 750 MiB of resident memory summed
			// per process, but 150 MiB actually used, well within the 320 MiB cap.
			const worker = [
				"import os, time",
				"b = bytearray(150 * 1024 * 1024)",
				"for i in range(0, len(b), 4096): b[i] = 1",
				"kids = []",
				"for _ in range(4):",
				"    pid = os.fork()",
				"    if pid == 0:",
				"        time.sleep(3)",
				"        os._exit(0)",
				"    kids.append(pid)",
				"for pid in kids: os.waitpid(pid, 0)",
				"print('done')",
			].join("\n");
			const result = await instance.execute(
				[
					"import subprocess, sys",
					`out = subprocess.run([sys.executable, '-c', ${JSON.stringify(worker)}], capture_output=True, text=True)`,
					"(out.returncode, out.stdout.strip())",
				].join("\n"),
			);
			expect(result).toMatchObject({ status: "ok", result: "(0, 'done')" });
		}, 30_000);
	});
}
