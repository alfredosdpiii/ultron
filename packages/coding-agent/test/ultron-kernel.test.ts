import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

describe("RLM kernel protocol", () => {
	test("preserves Python state and returns host replies and Python errors", async () => {
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async (type) => {
			if (type === "agents.list") return [{ id: "identity" }];
			throw new Error("Unknown request");
		});
		try {
			expect(await kernel.execute("answer = 42\nprint('ready')\nanswer")).toMatchObject({
				status: "ok",
				stdout: "ready\n",
				result: "42",
			});
			expect(await kernel.execute("answer + 1")).toMatchObject({ status: "ok", result: "43" });
			expect(await kernel.execute("await agents.list()")).toMatchObject({
				status: "ok",
				result: "[{'id': 'identity'}]",
			});
			expect(await kernel.execute("raise ValueError('bad')")).toMatchObject({
				status: "error",
				error: { ename: "ValueError", evalue: "bad" },
			});
		} finally {
			await kernel.shutdown();
		}
	});

	test("queued cancellation leaves the active cell running", async () => {
		let entered!: () => void;
		const enteredPromise = new Promise<void>((resolve) => {
			entered = resolve;
		});
		let release!: () => void;
		const blocked = new Promise<void>((resolve) => {
			release = resolve;
		});
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async () => {
			entered();
			await blocked;
			return "completed";
		});
		try {
			const active = kernel.execute("await agents.list()");
			await enteredPromise;
			const controller = new AbortController();
			const queued = kernel.execute("1 + 1", controller.signal);
			controller.abort(new Error("cancel queued"));
			await expect(queued).rejects.toThrow("cancel queued");
			release();
			expect(await active).toMatchObject({ status: "ok", result: "'completed'" });
			expect(await kernel.execute("6 * 7")).toMatchObject({ result: "42" });
		} finally {
			release();
			await kernel.shutdown();
		}
	});

	test("active cancellation stops Python and propagates to host work", async () => {
		let entered!: () => void;
		const started = new Promise<void>((resolve) => {
			entered = resolve;
		});
		let hostSignal: AbortSignal | undefined;
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async (_type, _payload, signal) => {
			hostSignal = signal;
			entered();
			await new Promise<void>((resolve) => signal?.addEventListener("abort", () => resolve(), { once: true }));
			return [];
		});
		try {
			const controller = new AbortController();
			const executing = kernel.execute("await agents.list()", controller.signal);
			await started;
			controller.abort(new Error("cancel active"));
			await expect(executing).rejects.toThrow("cancel active");
			expect(hostSignal?.aborted).toBe(true);
			expect(await kernel.execute("'fresh'")).toMatchObject({ status: "ok", result: "'fresh'" });
		} finally {
			await kernel.shutdown();
		}
	});

	test("rejects malformed protocol and startup stalls", async () => {
		const directory = await mkdtemp(join(tmpdir(), "ultron-kernel-"));
		try {
			const invalid = join(directory, "invalid.py");
			await writeFile(invalid, "print('not JSON', flush=True)\n");
			const kernel = new RlmKernel({ cwd: directory, runtimePath: invalid }, () => null);
			try {
				await expect(kernel.execute("1")).rejects.toThrow(/invalid protocol/);
			} finally {
				await kernel.shutdown();
			}
			const stalled = join(directory, "stalled.py");
			await writeFile(stalled, "import time\ntime.sleep(30)\n");
			const hanging = new RlmKernel({ cwd: directory, runtimePath: stalled, startupTimeoutMs: 100 }, () => null);
			try {
				await expect(hanging.execute("1")).rejects.toThrow(/startup timed out/);
			} finally {
				await hanging.shutdown();
			}
		} finally {
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("rejects oversized input without losing existing Python state", async () => {
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, () => null);
		try {
			await kernel.execute("answer = 42");
			await expect(kernel.execute(`#${"x".repeat(1024 * 1024)}`)).rejects.toThrow(/outbound frame exceeds/);
			expect(await kernel.execute("answer")).toMatchObject({ result: "42" });
		} finally {
			await kernel.shutdown();
		}
	});
});
