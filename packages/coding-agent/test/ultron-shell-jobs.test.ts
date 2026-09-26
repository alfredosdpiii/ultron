/**
 * Host-owned shell jobs: `await bash(cmd, yield_after=s)` runs the command in the host (the session worker), not in
 * the kernel. A running handle outlives the cell and a kernel restart, `result()` and `cancel()` work, the text is
 * bounded (head and tail around a marker, the rest readable from the spill file), an aborted root turn cancels its
 * jobs. Plain `await bash(cmd)` returns a string: finished within ULTRON_BASH_YIELD_AFTER it is the whole output,
 * else the command continues as a job (`.running`, `.job`, a note) and its end is announced; `yield_after=None` and
 * ULTRON_BASH_YIELD_AFTER=off block.
 */
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createLocalBashOperations } from "../src/core/tools/bash.ts";
import { runHostBash } from "../src/ultron/rlm/host-bash.ts";
import { createMemoryModuleStore, type NativeHostApi } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { JOB_TEXT_BYTES, jobSummary, type ShellJobEnd, ShellJobs } from "../src/ultron/rlm/shell-jobs.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

describe("host-owned shell jobs", () => {
	let cwd: string;
	let jobs: ShellJobs;
	let ends: ShellJobEnd[];
	let root: string | undefined;
	const kernels: RlmKernel[] = [];
	const store = createMemoryModuleStore();

	/** A kernel wired like the worker: `bash` with a yield_after runs as a job, else blocks; `shell.*` goes to the jobs module. */
	const kernel = (lane = "main", env: Record<string, string> = {}): RlmKernel => {
		const host = { rootOf: () => root } as unknown as NativeHostApi;
		const created = new RlmKernel({ cwd, runtimePath, env }, async (requested, payload, signal) => {
			const type = requested === "bash" && payload.yield_after !== undefined ? "shell.bash" : requested;
			if (type === "bash") return runHostBash(payload, cwd, createLocalBashOperations({}), signal);
			if (type.startsWith("shell."))
				return jobs.module.handle(
					{
						type,
						payload,
						caller: { lane },
						context: signal ? withAbortSignal(signal, BACKGROUND_CONTEXT) : BACKGROUND_CONTEXT,
					},
					host,
				);
			throw new Error(`unexpected host request ${type}`);
		});
		kernels.push(created);
		return created;
	};

	beforeEach(async () => {
		cwd = mkdtempSync(join(tmpdir(), "ultron-shell-jobs-"));
		ends = [];
		root = "turn:run-1";
		jobs = new ShellJobs({
			cwd,
			dir: join(cwd, ".jobs"),
			operations: () => createLocalBashOperations({}),
			store,
			retainedBytes: 64 * 1024,
			onEnd: (end) => ends.push(end),
		});
		await jobs.module.start?.({} as NativeHostApi);
	});

	afterEach(async () => {
		for (const created of kernels.splice(0)) await created.shutdown();
		await jobs.close();
		rmSync(cwd, { recursive: true, force: true });
	});

	test("yield_after returns a running handle that outlives the cell; result() waits and reports once", async () => {
		const k = kernel();
		const started = await k.execute(
			"job = await bash('''sleep 0.6; echo done-$((20+22))''', yield_after=0)\n(job.running, job.status, job.exit_code, job.id.startswith('job-'))",
		);
		expect(started).toMatchObject({ status: "ok", result: "(True, 'running', None, True)" });
		// The cell ended; the job keeps running in the host.
		expect(ends).toHaveLength(0);
		const finished = await k.execute(
			"r = await job.result()\n(r is job, job.running, job.ok, job.exit_code, job.text.strip())",
		);
		expect(finished).toMatchObject({ status: "ok", result: "(True, False, True, 0, 'done-42')" });
		// The cell was waiting when it ended: the result came inline, so no completion is announced.
		expect(ends).toEqual([expect.objectContaining({ awaited: true, rootAborted: false })]);
		expect(await k.execute("print(job)")).toMatchObject({ stdout: expect.stringContaining("done-42\n[job ") });
	});

	test("an unawaited job reports its end once, with a bounded summary; a quick one finishes inline", async () => {
		const k = kernel();
		await k.execute("job = await bash('''sleep 0.3; echo line one; echo 12 passed''', yield_after=0)");
		await expect.poll(() => ends.length, { timeout: 5000 }).toBe(1);
		expect(ends[0]).toMatchObject({ awaited: false, rootAborted: false, job: { status: "completed", exitCode: 0 } });
		expect(jobSummary(ends[0]!.job)).toBe("exit 0; line one | 12 passed");
		expect(ends[0]!.job.rootId).toBe("turn:run-1");
		const quick = await k.execute("q = await bash('''echo quick''', yield_after=5)\n(q.running, q.ok, q.text)");
		expect(quick).toMatchObject({ status: "ok", result: "(False, True, 'quick\\n')" });
		expect(ends.filter((end) => !end.awaited)).toHaveLength(1);
	});

	test("a job survives a kernel restart and is recovered by id; rlm.jobs() lists it", async () => {
		const first = kernel();
		const started = await first.execute("job = await bash('''sleep 0.5; echo survived''', yield_after=0); job.id");
		const id = JSON.parse(started.result!.replace(/'/g, '"')) as string;
		await first.shutdown();
		const second = kernel();
		expect(await second.execute("'job' in globals()")).toMatchObject({ result: "False" });
		const recovered = await second.execute(
			`j = await rlm.job(${JSON.stringify(id)})\nawait j.result()\n(j.ok, j.text.strip(), [x.id for x in await rlm.jobs()][:1] == [j.id])`,
		);
		expect(recovered).toMatchObject({ status: "ok", result: "(True, 'survived', True)" });
		expect(await second.execute("(await rlm.jobs())[0]")).toMatchObject({
			result: expect.stringContaining(`ShellJob(id='${id}', status='completed', exit_code=0`),
		});
	});

	test("cancel() stops the process tree; timeout= reports timed_out; wait= returns a running snapshot", async () => {
		const k = kernel();
		const pidFile = join(cwd, "child.pid");
		await k.execute(`job = await bash('''sleep 30 & echo $! > ${pidFile}; wait''', yield_after=0.3)`);
		const pid = Number(readFileSync(pidFile, "utf8").trim());
		expect(await k.execute("s = await job.result(wait=0.2); (s.running, s.status)")).toMatchObject({
			result: "(True, 'running')",
		});
		expect(
			await k.execute("await job.cancel(); (job.running, job.cancelled, job.status, job.exit_code)"),
		).toMatchObject({ result: "(False, True, 'cancelled', None)" });
		await expect.poll(() => alive(pid), { timeout: 5000 }).toBe(false);
		const timed = await k.execute(
			"t = await bash('''sleep 10''', timeout=0.3, yield_after=0)\nawait t.result()\n(t.timed_out, t.ok, t.status)",
		);
		expect(timed).toMatchObject({ status: "ok", result: "(True, False, 'timed_out')" });
	});

	test("output is bounded: head and tail with a marker, the rest readable, retention capped", async () => {
		const k = kernel();
		const result = await k.execute(
			"big = await bash('''for i in $(seq 1 20000); do echo line-$i; done''', yield_after=30)\n(big.truncated, len(big.text.encode()) < 17000, big.text.startswith('line-1\\n'), big.text.rstrip().endswith('line-20000'), 'bytes omitted' in big.text, big.output_bytes)",
		);
		expect(result.status).toBe("ok");
		expect(result.result).toMatch(/^\(True, True, True, True, True, \d+\)$/);
		const chunk = await k.execute(
			"c = await big.read(cursor=0, max_bytes=100); (c['text'][:7], c['next_cursor'], c['truncated'])",
		);
		expect(chunk).toMatchObject({ result: "('line-1\\n', 100, True)" });
		// The spill file keeps the head up to the retention cap (64 KiB in this test).
		const path = (await k.execute("big.output_path")).result!.replace(/'/g, "");
		expect(readFileSync(path).length).toBe(64 * 1024);
		expect(JOB_TEXT_BYTES).toBe(16 * 1024);
	});

	test("an aborted root turn cancels its jobs and announces nothing; other roots' jobs keep running", async () => {
		const k = kernel();
		await k.execute("a = await bash('''sleep 30''', yield_after=0)");
		root = "turn:run-2";
		await k.execute("b = await bash('''sleep 0.8; echo b''', yield_after=0)");
		await jobs.cancelRoot("turn:run-1");
		expect(await k.execute("(await rlm.job(a.id)).status")).toMatchObject({ result: "'cancelled'" });
		expect(ends).toEqual([
			expect.objectContaining({ rootAborted: true, job: expect.objectContaining({ rootId: "turn:run-1" }) }),
		]);
		await expect.poll(() => ends.length, { timeout: 5000 }).toBe(2);
		expect(ends[1]).toMatchObject({ rootAborted: false, job: { status: "completed", rootId: "turn:run-2" } });
	});

	test("a child lane sees only its own jobs; plain bash still blocks and returns a string", async () => {
		const main = kernel("main");
		const child = kernel("ultron.rlm-child.task-1");
		await main.execute("m = await bash('''echo main''', yield_after=1)");
		const id = (await main.execute("m.id")).result!.replace(/'/g, "");
		expect(await child.execute("await rlm.jobs()")).toMatchObject({ result: "[]" });
		expect(await child.execute(`await rlm.job('${id}')`)).toMatchObject({
			status: "error",
			error: { evalue: expect.stringContaining(`Unknown shell job ${id}`) },
		});
		expect(await main.execute(`(await rlm.job('${id}')).ok`)).toMatchObject({ result: "True" });
		expect(
			await main.execute("out = await bash('''echo plain; exit 2'''); (type(out).__name__, str(out))"),
		).toMatchObject({
			result: "('BashOutput', 'plain\\n[exit code 2]')",
		});
		expect(existsSync(join(cwd, ".jobs"))).toBe(true);
	});

	test("plain bash auto-detaches a slow command: a running string with its job and a note, then one event", async () => {
		const k = kernel("main", { ULTRON_BASH_YIELD_AFTER: "0.5" });
		const out = await k.execute(
			"out = await bash('''echo early; sleep 1.5; echo late-$((40+2))''')\n(isinstance(out, str), type(out).__name__, out.running, out.ok, out.exit_code, out.job.id.startswith('job-'), out.job.running)",
		);
		expect(out).toMatchObject({ status: "ok", result: "(True, 'BashOutput', True, False, None, True, True)" });
		const text = (await k.execute("print(out)")).stdout;
		expect(text).toMatch(
			/^early\n\[still running as job job-[0-9a-f]+ after 0\.5 s; its completion will arrive as a runtime event, or `await <result>\.job\.result\(\)` to wait\]\n$/,
		);
		expect(await k.execute("(out['running'], out.get('job') is out.job)")).toMatchObject({ result: "(True, True)" });
		// Nobody waits on it: its end is announced once, as for any detached job.
		await expect.poll(() => ends.length, { timeout: 5000 }).toBe(1);
		expect(ends[0]).toMatchObject({ awaited: false, job: { status: "completed", exitCode: 0 } });
		expect(jobSummary(ends[0]!.job)).toBe("exit 0; early | late-42");
		expect(
			await k.execute("j = await out.job.result(); (j.running, j.ok, j.exit_code, j.text.split())"),
		).toMatchObject({ result: "(False, True, 0, ['early', 'late-42'])" });
	});

	test("plain bash that finishes within the window returns the whole output and announces nothing", async () => {
		const k = kernel("main", { ULTRON_BASH_YIELD_AFTER: "20" });
		expect(
			await k.execute(
				"out = await bash('''echo plain; exit 2'''); (str(out), out.exit_code, out.ok, out.running, out.job)",
			),
		).toMatchObject({ result: "('plain\\n[exit code 2]', 2, False, False, None)" });
		// Output over the 16 KiB job text comes back whole from the spill file.
		const big = await k.execute(
			"big = await bash('''for i in $(seq 1 3000); do echo line-$i; done'''); (big.truncated, len(big.splitlines()), big.splitlines()[-1])",
		);
		expect(big).toMatchObject({ result: "(False, 3000, 'line-3000')" });
		// A timeout still reports as the blocking bash did.
		expect(
			await k.execute(
				"t = await bash('''sleep 5''', timeout=0.3); (t.timed_out, t.ok, str(t).endswith('[timed out after 0.3s]'))",
			),
		).toMatchObject({ result: "(True, False, True)" });
		expect(ends).toHaveLength(3);
		expect(ends.every((end) => end.awaited)).toBe(true);
	});

	test("yield_after=None and ULTRON_BASH_YIELD_AFTER=off block until the command ends; no job is made", async () => {
		const k = kernel("main", { ULTRON_BASH_YIELD_AFTER: "0.2" });
		const before = jobs.list().length;
		const started = Date.now();
		expect(
			await k.execute("b = await bash('''sleep 0.8; echo blocked''', yield_after=None); (str(b), b.running, b.ok)"),
		).toMatchObject({ result: "('blocked', False, True)" });
		expect(Date.now() - started).toBeGreaterThanOrEqual(700);
		const off = kernel("main", { ULTRON_BASH_YIELD_AFTER: "off" });
		expect(await off.execute("b = await bash('''sleep 0.5; echo off'''); (str(b), b.running)")).toMatchObject({
			result: "('off', False)",
		});
		expect(jobs.list()).toHaveLength(before);
	});

	test("the journal stays small and survives a worker restart: finished jobs keep their text, running ones read as interrupted", async () => {
		const journal = createMemoryModuleStore();
		const first = new ShellJobs({
			cwd,
			dir: join(cwd, ".jobs2"),
			operations: () => createLocalBashOperations({}),
			store: journal,
		});
		await first.module.start?.({} as NativeHostApi);
		const done = first.start("main", null, "echo kept-output", undefined);
		await done.done;
		first.start("main", null, "sleep 30", undefined);
		await first.settled();
		const saved = JSON.stringify(await journal.read());
		// Output lives in the spill files, not in the journal that is rewritten with every job.
		expect(saved).not.toContain("kept-output\\n");
		expect(saved.length).toBeLessThan(2000);
		const second = new ShellJobs({
			cwd,
			dir: join(cwd, ".jobs2"),
			operations: () => createLocalBashOperations({}),
			store: journal,
		});
		await second.module.start?.({} as NativeHostApi);
		const [running, finished] = second.list();
		expect(running).toMatchObject({ status: "interrupted", error: "worker restarted" });
		expect(second.snapshot(finished!)).toMatchObject({ status: "completed", ok: true, text: "kept-output\n" });
		expect(jobSummary(finished!)).toBe("exit 0; kept-output");
		await first.close();
		await second.close();
	});
});

function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch {
		return false;
	}
}
