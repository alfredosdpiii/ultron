/**
 * The kernel's protocol runs on private descriptors (fd 3 out, fd 4 in; Linux and macOS), so fd 1 and fd 2 are
 * plain output. Raw writes that bypass Python's sys.stdout (os.write, os.system, uncaptured subprocesses, the
 * original sys.__stdout__) land in the running cell's result instead of corrupting the protocol and ending the
 * kernel. A flush marker the runtime writes after each cell makes that output complete and deterministic.
 */
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

describe.skipIf(process.platform === "win32")("RLM kernel raw output on fd 1 and fd 2", () => {
	async function withKernel(run: (kernel: RlmKernel, pid: string) => Promise<void>): Promise<void> {
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, () => null);
		try {
			const first = await kernel.execute("import os, subprocess, sys\nkept = 'intact'\nos.getpid()");
			// Interpreter output before the runtime is ready (warnings, say) is startup diagnostics, not cell output.
			expect(first).toMatchObject({ status: "ok", stdout: "", stderr: "" });
			const pid = first.result!;
			await run(kernel, pid);
			// The same kernel process and its variables survive every raw write.
			expect(await kernel.execute("os.getpid(), kept")).toMatchObject({
				status: "ok",
				result: `(${pid}, 'intact')`,
			});
		} finally {
			await kernel.shutdown();
		}
	}

	test("os.write(1) of 3 MB lands whole in the cell's stdout", async () => {
		await withKernel(async (kernel) => {
			const result = await kernel.execute("os.write(1, b'<' + b'x' * 3_000_000 + b'>')\n'done'");
			expect(result).toMatchObject({ status: "ok", result: "'done'" });
			expect(result.stdout).toHaveLength(3_000_002);
			expect(result.stdout.startsWith("<xxx")).toBe(true);
			expect(result.stdout.endsWith("xxx>")).toBe(true);
		});
	}, 60_000);

	test("raw output over the 4 MiB per-cell limit keeps its head and tail with a marker", async () => {
		await withKernel(async (kernel) => {
			const result = await kernel.execute("os.write(1, b'H' + b'y' * 6_000_000 + b'T')\n1");
			expect(result).toMatchObject({ status: "ok", result: "1" });
			expect(result.stdout.length).toBeLessThanOrEqual(4 * 1024 * 1024 + 256);
			expect(result.stdout.startsWith("Hyyy")).toBe(true);
			expect(result.stdout.endsWith("yyyT")).toBe(true);
			expect(result.stdout).toMatch(
				/raw fd 1 output cut from the middle: over the RLM kernel's 4 MiB per-cell output limit/,
			);
		});
	}, 60_000);

	test("os.system, an uncaptured subprocess and sys.__stdout__ write into the cell", async () => {
		await withKernel(async (kernel) => {
			const shell = await kernel.execute("os.system('yes | head -c 2000000')");
			expect(shell).toMatchObject({ status: "ok", result: "0" });
			expect(shell.stdout).toHaveLength(2_000_000);
			expect(shell.stdout.startsWith("y\ny\n")).toBe(true);

			const child = await kernel.execute(
				"subprocess.run([sys.executable, '-c', \"print('hi' * 500000)\"]).returncode",
			);
			expect(child).toMatchObject({ status: "ok", result: "0" });
			expect(child.stdout).toBe(`${"hi".repeat(500_000)}\n`);

			// sys.__stdout__ is buffered by Python; the runtime flushes it before the cell's marker.
			const original = await kernel.execute("print('via __stdout__', file=sys.__stdout__)");
			expect(original).toMatchObject({ status: "ok" });
			expect(original.stdout).toBe("via __stdout__\n");
		});
	}, 60_000);

	test("output C code buffers in stdio is flushed into the cell", async () => {
		await withKernel(async (kernel) => {
			// fd 1 is a pipe, so libc buffers printf output; the runtime flushes C stdio before the marker.
			const result = await kernel.execute("import ctypes\nctypes.CDLL(None).printf(b'from C %d\\n', 42)\nNone");
			expect(result.status).toBe("ok");
			expect(result.stdout).toBe("from C 42\n");
		});
	}, 30_000);

	test("writes to fd 2 land in the cell's stderr", async () => {
		await withKernel(async (kernel) => {
			const result = await kernel.execute(
				[
					"os.write(2, b'raw error\\n')",
					"print('buffered error', file=sys.__stderr__)",
					"subprocess.run(['sh', '-c', 'echo from child >&2'])",
					"None",
				].join("\n"),
			);
			expect(result.status).toBe("ok");
			expect(result.stdout).toBe("");
			expect(result.stderr).toBe("raw error\nbuffered error\nfrom child\n");
			const flood = await kernel.execute("os.write(2, b'e' * 3_000_000)\n2");
			expect(flood).toMatchObject({ status: "ok", result: "2" });
			expect(flood.stderr).toHaveLength(3_000_000);
		});
	}, 60_000);

	test("print() and raw writes in one cell both reach the result, deterministically", async () => {
		await withKernel(async (kernel) => {
			for (let round = 0; round < 20; round++) {
				const result = await kernel.execute(
					[
						"for i in range(3):",
						"    print(f'print {i}')",
						"    os.write(1, f'raw {i}\\n'.encode())",
						"    subprocess.run(['echo', f'child {i}'])",
					].join("\n"),
				);
				expect(result.status).toBe("ok");
				// Python's captured print output comes first, then the fd 1 output in the order it was written.
				expect(result.stdout).toBe("print 0\nprint 1\nprint 2\nraw 0\nchild 0\nraw 1\nchild 1\nraw 2\nchild 2\n");
			}
		});
	}, 60_000);

	test("a failing cell still collects its raw output", async () => {
		await withKernel(async (kernel) => {
			const result = await kernel.execute("os.write(1, b'before failure\\n')\nraise ValueError('boom')");
			expect(result.status).toBe("error");
			expect(result.error?.ename).toBe("ValueError");
			expect(result.stdout).toBe("before failure\n");
		});
	}, 30_000);

	test("output from outside a cell is reported, bounded, at the start of the next cell", async () => {
		await withKernel(async (kernel) => {
			// A background process keeps writing after its cell has finished.
			const directory = await mkdtemp(join(tmpdir(), "ultron-raw-"));
			const written = join(directory, "written");
			try {
				const started = await kernel.execute(
					`background = subprocess.Popen(['sh', '-c', 'sleep 0.2; echo late output; touch ${written}'])\n'started'`,
				);
				expect(started).toMatchObject({ status: "ok", stdout: "", result: "'started'" });
				// Let the host read the late output while no cell runs.
				while (!existsSync(written)) await new Promise((resolve) => setTimeout(resolve, 20));
				await new Promise((resolve) => setTimeout(resolve, 300));
			} finally {
				await rm(directory, { recursive: true, force: true });
			}
			const next = await kernel.execute("background.wait()\nprint('next cell')");
			expect(next.status).toBe("ok");
			expect(next.stdout).toContain("[kernel output written outside a cell, before this one");
			expect(next.stdout).toContain("late output\n[end of output from outside a cell]\n");
			expect(next.stdout.endsWith("next cell\n")).toBe(true);
			// It is reported once.
			const after = await kernel.execute("print('clean')");
			expect(after.stdout).toBe("clean\n");
		});
	}, 30_000);

	test("processes the kernel starts cannot reach the protocol descriptors", async () => {
		await withKernel(async (kernel) => {
			const result = await kernel.execute(
				[
					"probe = \"import os\\nfor fd in (3, 4):\\n    try:\\n        os.fstat(fd); print(fd, 'open')\\n    except OSError: print(fd, 'closed')\"",
					"inherited = subprocess.run([sys.executable, '-c', probe], close_fds=False, capture_output=True, text=True).stdout",
					"shell = os.system('echo junk >&3 2>/dev/null; echo junk >&4 2>/dev/null')",
					"inherited, shell != 0, os.get_inheritable(3), os.get_inheritable(4)",
				].join("\n"),
			);
			expect(result).toMatchObject({ status: "ok", result: "('3 closed\\n4 closed\\n', True, False, False)" });
		});
	}, 30_000);

	test("stdin is empty: input() fails fast with a clear error, and children read EOF", async () => {
		await withKernel(async (kernel) => {
			const result = await kernel.execute("input('name? ')");
			expect(result.status).toBe("error");
			expect(result.error?.ename).toBe("EOFError");
			expect(result.error?.evalue).toMatch(/no interactive stdin/);
			const child = await kernel.execute("subprocess.run(['cat'], capture_output=True, timeout=10).stdout");
			expect(child).toMatchObject({ status: "ok", result: "b''" });
			expect(await kernel.execute("sys.stdin.readline()")).toMatchObject({ status: "ok", result: "''" });
		});
	}, 30_000);

	test("the stdio protocol channel still works where fd 3 is not used", async () => {
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath, protocolChannel: "stdio" }, () => null);
		try {
			expect(await kernel.execute("print('hello')\n6 * 7")).toMatchObject({
				status: "ok",
				stdout: "hello\n",
				result: "42",
			});
		} finally {
			await kernel.shutdown();
		}
	}, 30_000);
});
