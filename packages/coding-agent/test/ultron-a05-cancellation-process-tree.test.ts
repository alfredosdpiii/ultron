import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

// A05: cancellation terminates owned work and rejects late success.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const context = {} as never;
const directories: string[] = [];

afterEach(() => {
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function scratch(): string {
	const directory = mkdtempSync(join(tmpdir(), "ultron-a05-"));
	directories.push(directory);
	return directory;
}

type TreePids = { kernel: number; child: number; grandchild: number; detached: number };

/** Python cell: child process with a same-group grandchild and a setsid grandchild, then a long wait. */
function processTreeCell(pidFile: string, wait: string): string {
	return [
		"import json, os, subprocess, sys",
		"child_code = '''",
		"import subprocess, sys, time",
		"g1 = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])",
		"g2 = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True)",
		"print(g1.pid, g2.pid, flush=True)",
		"time.sleep(120)",
		"'''",
		"proc = subprocess.Popen([sys.executable, '-c', child_code], stdout=subprocess.PIPE, text=True)",
		"grandchild, detached = map(int, proc.stdout.readline().split())",
		`open(${JSON.stringify(pidFile)}, 'w').write(json.dumps({'kernel': os.getpid(), 'child': proc.pid, 'grandchild': grandchild, 'detached': detached}))`,
		wait,
		"'finished'",
	].join("\n");
}

/** True for a running process; a zombie awaiting its reaper no longer runs anything. */
function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
	} catch {
		return false;
	}
	try {
		const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
		return stat.slice(stat.lastIndexOf(")") + 2)[0] !== "Z";
	} catch {
		return false;
	}
}

async function waitFor(condition: () => boolean, timeoutMs = 10_000): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	while (!condition()) {
		if (Date.now() > deadline) throw new Error("condition not reached in time");
		await new Promise((resolve) => setTimeout(resolve, 25));
	}
}

async function treePids(pidFile: string): Promise<TreePids> {
	await waitFor(() => existsSync(pidFile) && readFileSync(pidFile, "utf8").length > 0);
	return JSON.parse(readFileSync(pidFile, "utf8")) as TreePids;
}

function memoryStore(): NativeHostStore & { history: unknown[] } {
	let value: unknown;
	const history: unknown[] = [];
	return {
		history,
		read: async () => value as never,
		write: async (next) => {
			value = structuredClone(next);
			history.push(structuredClone(next));
		},
	};
}

const reviewerAnswer = {
	id: "tip",
	type: "message",
	message: { role: "assistant", content: [{ type: "text", text: '{"outcome":"no_findings","findings":[]}' }] },
};

type Gate = { promise: Promise<void>; open(): void };
function gate(): Gate {
	let open!: () => void;
	const promise = new Promise<void>((resolve) => {
		open = resolve;
	});
	return { promise, open };
}

/** A lane whose provider answers successfully only after `delay` resolves, ignoring abort. */
function delayedLane(delay: () => Promise<void>) {
	let aborts = 0;
	let prompts = 0;
	return {
		get aborts() {
			return aborts;
		},
		get prompts() {
			return prompts;
		},
		findEntries: async () => [reviewerAnswer],
		getActiveTools: async () => [],
		setActiveTools: async () => {},
		setModel: async () => {},
		prompt: async () => {
			prompts += 1;
			await delay();
			return { ok: true, value: { status: "completed", tipId: "tip" } };
		},
		abort: async () => {
			aborts += 1;
			return { ok: true };
		},
	};
}

function taskStates(history: unknown[], id: string): Array<{ state: string; status?: string }> {
	return history.flatMap((document) => {
		const task = (
			document as { tasks: Array<{ id: string; state: string; result?: { status: string } }> }
		).tasks.find((candidate) => candidate.id === id);
		return task ? [{ state: task.state, status: task.result?.status }] : [];
	});
}

describe.skipIf(process.platform !== "linux")("A05 cancellation terminates owned work and rejects late success", () => {
	test("cancelling a running cell kills its whole process tree, including a setsid grandchild", async () => {
		const pidFile = join(scratch(), "pids.json");
		let hostSignal: AbortSignal | undefined;
		let lateReply: (() => void) | undefined;
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, (_type, _payload, signal) => {
			hostSignal = signal;
			// Delayed provider: answers only well after cancellation, ignoring the abort signal.
			return new Promise((resolve) => {
				lateReply = () => resolve({ late: "success" });
			});
		});
		try {
			const controller = new AbortController();
			const running = kernel.execute(
				processTreeCell(pidFile, "late = await rlm.host_request('provider.call', {})"),
				controller.signal,
			);
			const pids = await treePids(pidFile);
			await waitFor(() => hostSignal !== undefined);
			for (const pid of Object.values(pids)) expect(alive(pid)).toBe(true);

			controller.abort(new Error("user cancelled"));
			await expect(running).rejects.toThrow("user cancelled");
			expect(hostSignal?.aborted).toBe(true);
			await waitFor(() => Object.values(pids).every((pid) => !alive(pid)));

			// The late provider success arrives after cancellation and must not revive the cell.
			lateReply?.();
			await new Promise((resolve) => setTimeout(resolve, 50));
			await expect(running).rejects.toThrow("user cancelled");
			const fresh = await kernel.execute("'late' in globals()");
			expect(fresh).toMatchObject({ status: "ok", result: "False" });
		} finally {
			await kernel.shutdown();
		}
	});

	test("shutdown also terminates the owned tree of a cell blocked in synchronous code", async () => {
		const pidFile = join(scratch(), "pids.json");
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, () => null);
		const running = kernel.execute(processTreeCell(pidFile, "import time\ntime.sleep(120)"));
		void running.catch(() => {});
		const pids = await treePids(pidFile);
		await kernel.shutdown();
		await expect(running).rejects.toThrow("shut down");
		await waitFor(() => Object.values(pids).every((pid) => !alive(pid)));
	});

	test("cancelling a native task aborts its lane, kills the lane kernel's process tree, and keeps the durable cancelled result", async () => {
		const pidFile = join(scratch(), "pids.json");
		const store = memoryStore();
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, () => null);
		let controller = new AbortController();
		let resumeProvider!: () => void;
		const providerGate = new Promise<void>((resolve) => {
			resumeProvider = resolve;
		});
		const lane = {
			...delayedLane(async () => {}),
			prompt: async () => {
				controller = new AbortController();
				// The lane's work runs Python that owns a process tree.
				await kernel
					.execute(processTreeCell(pidFile, "import asyncio\nawait asyncio.sleep(120)"), controller.signal)
					.catch(() => {});
				// A provider success arrives after the kernel was killed.
				await providerGate;
				return { ok: true, value: { status: "completed", tipId: "tip" } };
			},
			abort: async () => {
				controller.abort(new Error("lane aborted"));
				return { ok: true };
			},
		};
		const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store });
		try {
			const spawned = (await host.handle(
				"agents.spawn",
				{ definition: "security-reviewer@1", input: { request: "review" } },
				context,
			)) as { id: string };
			const pids = await treePids(pidFile);
			for (const pid of Object.values(pids)) expect(alive(pid)).toBe(true);

			await expect(host.handle("agents.cancel", { id: spawned.id }, context)).resolves.toEqual({ cancelled: true });
			await waitFor(() => Object.values(pids).every((pid) => !alive(pid)));

			resumeProvider();
			await new Promise((resolve) => setTimeout(resolve, 50));
			expect(await host.handle("agents.result", { id: spawned.id }, context)).toMatchObject({ status: "cancelled" });
			const states = taskStates(store.history, spawned.id);
			expect(states.at(-1)).toEqual({ state: "cancelled", status: "cancelled" });
			expect(states.some((state) => state.state === "completed")).toBe(false);

			const reopened = new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store });
			expect(await reopened.handle("agents.inspect", { id: spawned.id }, context)).toMatchObject({
				state: "cancelled",
				result: { status: "cancelled" },
			});
			await reopened.close();
		} finally {
			resumeProvider();
			await host.close();
			await kernel.shutdown();
		}
	});

	test("a delayed provider success after cancellation never overwrites the durable cancelled result", async () => {
		const store = memoryStore();
		const provider = gate();
		const lane = delayedLane(() => provider.promise);
		const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store });
		try {
			const spawned = (await host.handle(
				"agents.spawn",
				{ definition: "security-reviewer@1", input: { request: "review" } },
				context,
			)) as { id: string };
			await waitFor(() => lane.prompts === 1);
			await expect(host.handle("agents.cancel", { id: spawned.id }, context)).resolves.toEqual({ cancelled: true });
			await waitFor(() => lane.aborts > 0);

			provider.open();
			await new Promise((resolve) => setTimeout(resolve, 50));
			expect(await host.handle("agents.result", { id: spawned.id }, context)).toMatchObject({
				status: "cancelled",
				verification: "unverified",
			});
			const states = taskStates(store.history, spawned.id);
			expect(states.filter((state) => state.state === "cancelled")).toHaveLength(1);
			expect(states.some((state) => state.state === "completed")).toBe(false);
		} finally {
			provider.open();
			await host.close();
		}
	});

	test("racing cancellation against provider success always yields one durable, consistent terminal result", async () => {
		const outcomes = new Set<string>();
		for (let iteration = 0; iteration < 30; iteration += 1) {
			const store = memoryStore();
			const lane = delayedLane(() => new Promise((resolve) => setTimeout(resolve, iteration % 4)));
			const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store });
			try {
				const spawned = (await host.handle(
					"agents.spawn",
					{ definition: "security-reviewer@1", input: { request: "review" } },
					context,
				)) as { id: string };
				await new Promise((resolve) => setTimeout(resolve, (iteration * 3) % 5));
				const cancelled = (await host.handle("agents.cancel", { id: spawned.id }, context)) as {
					cancelled: boolean;
				};
				const result = (await host.handle("agents.result", { id: spawned.id }, context)) as { status: string };
				// Wait out any late completion, then check that nothing replaced the first terminal record.
				await new Promise((resolve) => setTimeout(resolve, 10));
				const states = taskStates(store.history, spawned.id);
				const terminal = states.filter((state) => ["completed", "cancelled", "failed"].includes(state.state));
				expect(new Set(terminal.map((state) => state.state)).size).toBe(1);
				expect(result.status).toBe(cancelled.cancelled ? "cancelled" : "succeeded");
				expect(terminal.at(-1)?.status).toBe(result.status);
				expect(await host.handle("agents.result", { id: spawned.id }, context)).toMatchObject({
					status: result.status,
				});
				outcomes.add(result.status);
			} finally {
				await host.close();
			}
		}
		// The fixture exercises the cancellation side of the race, not only late completion.
		expect(outcomes.has("cancelled")).toBe(true);
	});
});
