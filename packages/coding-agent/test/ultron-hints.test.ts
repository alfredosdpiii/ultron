/**
 * Situational hints and the `read` skill:
 * - each trigger (job-detached, blocked-on-job, poll-loop, output-truncated, large-read, repeated-failure) adds one
 *   `[hint:<tag>]` line to the cell's result; at most one per cell, the most specific first;
 * - `hints.mute`/`unmute`/`muted` work per lane from the kernel and persist; each tag fires at most N times per lane;
 *   ULTRON_HINTS=off disables hints;
 * - `await read(path)` returns text up to ULTRON_READ_HANDLE_BYTES and a ContextHandle (with a note) above it.
 */
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import {
	CellHints,
	type CellHintsOptions,
	hintMaxPerTag,
	hintsEnabled,
	readHandleBytes,
} from "../src/ultron/rlm/hints.ts";
import { createMemoryModuleStore, type HostModuleStore, type NativeHostApi } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const TRUNCATED = "head\n[... 5000 bytes truncated ...]\ntail";

describe("situational hints", () => {
	let clock: number;
	let store: HostModuleStore;
	const make = (options: Partial<CellHintsOptions> = {}): CellHints =>
		new CellHints({ store, now: () => clock, ...options });
	/** Run one "cell": the observed host requests, then its outcome; returns the hint line. */
	const cell = (
		hints: CellHints,
		code: string,
		requests: Array<[string, Record<string, unknown>, unknown, number?]> = [],
		outcome: { text?: string; ename?: string } = {},
		lane = "main",
	): Promise<string | undefined> => {
		hints.beginCell(lane, code);
		for (const [type, payload, result, tookMs] of requests) {
			const startedAt = clock;
			clock += tookMs ?? 10;
			hints.observe(lane, type, payload, result, startedAt);
		}
		return hints.endCell(lane, { text: outcome.text ?? "", ...(outcome.ename ? { ename: outcome.ename } : {}) });
	};
	const mute = (hints: CellHints, type: string, tags: string[], lane = "main") =>
		hints.module.handle(
			{ type, payload: { tags }, caller: { lane }, context: BACKGROUND_CONTEXT },
			{} as NativeHostApi,
		) as Promise<{ muted: string[] }>;

	beforeEach(() => {
		clock = 1_000_000;
		store = createMemoryModuleStore();
	});

	test("job-detached: a plain bash that came back running", async () => {
		const hints = make();
		const hint = await cell(hints, "out = await bash('pytest')", [
			["bash", { command: "pytest", yield_after: 30 }, { running: true, job: { id: "job-ab12" } }],
		]);
		expect(hint).toMatch(
			/^\[hint:job-detached\] The command was still running after 30 s, so it continues as job job-ab12\./,
		);
		expect(hint).toContain("<runtime_event>");
		expect(hint).toContain('await hints.mute("job-detached")');
		expect(hint!.split("\n")).toHaveLength(1);
		// A command that finished inside the window is not a detach.
		expect(
			await cell(hints, "await bash('ls')", [["bash", { yield_after: 30 }, { running: false, job: null }]]),
		).toBe(undefined);
		// With events off the hint says how to wait instead.
		const quiet = make({ asyncEvents: false });
		const off = await cell(quiet, "x", [["bash", { yield_after: 30 }, { running: true, job: { id: "job-1" } }]]);
		expect(off).toContain("`await <result>.job.result()` waits for it");
		expect(off).not.toContain("runtime_event");
	});

	test("blocked-on-job: over 60 s awaiting job.result / rlm.collect / agents.result while events are on", async () => {
		const hints = make();
		expect(
			await cell(hints, "await job.result()", [["shell.result", { id: "job-1" }, { running: false }, 59_000]]),
		).toBe(undefined);
		const hint = await cell(hints, "await rlm.collect()", [
			["rlm.collect", { selectors: [] }, { results: [] }, 40_000],
			["agents.result", { id: "t-1" }, {}, 25_000],
		]);
		expect(hint).toMatch(
			/^\[hint:blocked-on-job\] This cell spent 65 s waiting in rlm\.collect\(\), agents\.result\(\)\./,
		);
		expect(hint).toContain("You could have ended your turn");
		// Concurrent waits (asyncio.gather) count once.
		const gathered = make({ store: createMemoryModuleStore() });
		gathered.beginCell("main", "await asyncio.gather(a.result(), b.result())");
		gathered.observe("main", "shell.result", { id: "a" }, { running: false }, clock);
		gathered.observe("main", "shell.result", { id: "b" }, { running: false }, clock);
		clock += 40_000;
		expect(await gathered.endCell("main", { text: "" })).toBe(undefined);
		// Without completion events waiting is the only way, so no hint.
		const quiet = make({ asyncEvents: false, store: createMemoryModuleStore() });
		expect(await cell(quiet, "x", [["shell.result", { id: "j" }, { running: false }, 120_000]])).toBe(undefined);
	});

	test("poll-loop: repeated status calls in a cell or across cells, or sleeping in a loop around a check", async () => {
		const hints = make({ maxPerTag: 10 });
		const running = { running: true };
		const within = await cell(hints, "x", [
			["shell.get", { id: "job-1" }, running],
			["shell.get", { id: "job-1" }, running],
			["shell.get", { id: "job-1" }, running],
		]);
		expect(within).toMatch(/^\[hint:poll-loop\] Repeated status checks \(shell\.get job-1\) look like polling\./);
		expect(within).toContain("Completions arrive as <runtime_event> messages on their own");
		// Consecutive cells asking the same status.
		expect(await cell(hints, "await agents.status()", [["agents.status", {}, {}]])).toBe(undefined);
		expect(await cell(hints, "await agents.status()", [["agents.status", {}, {}]])).toContain("[hint:poll-loop]");
		// A result fetch of finished work is not a poll.
		expect(await cell(hints, "a", [["shell.result", { id: "j" }, { running: false }]])).toBe(undefined);
		expect(await cell(hints, "b", [["shell.result", { id: "j" }, { running: false }]])).toBe(undefined);
		// Sleep in a loop with a status check.
		const loop = "while True:\n    s = await rlm.job(j)\n    if not s.running: break\n    await asyncio.sleep(5)";
		expect(await cell(hints, loop)).toMatch(/^\[hint:poll-loop\] Sleeping in a loop around a status check/);
		expect(await cell(hints, "for f in files:\n    await asyncio.sleep(0.1)\n    print(f)")).toBe(undefined);
	});

	test("output-truncated and large-read: cut output, and a big file read into a string", async () => {
		const hints = make();
		expect(await cell(hints, "print(data)", [], { text: TRUNCATED })).toMatch(
			/^\[hint:output-truncated\] The output was cut/,
		);
		// A file read in the same cell makes it the more specific large-read.
		expect(await cell(hints, "print(open('big.log').read())", [], { text: TRUNCATED })).toMatch(
			/^\[hint:large-read\] This cell read a large file into a string\. `await read\(path\)` returns a ContextHandle for files over 256 KiB/,
		);
		// A by-reference string over the threshold from a read, without any truncation.
		const shown = "<str: 900,000 chars, 12,000 lines, sha256 0123456789ab>\nhead: 'x'";
		expect(await cell(hints, "Path('big.log').read_text()", [], { text: shown })).toContain("[hint:large-read]");
		expect(await cell(hints, "'x' * 900000", [], { text: shown })).toBe(undefined);
		expect(await cell(hints, "Path('small').read_text()", [], { text: "<str: 3,000 chars, 1 lines" })).toBe(
			undefined,
		);
	});

	test("repeated-failure: the same exception type three cells in a row", async () => {
		const hints = make();
		expect(await cell(hints, "a", [], { ename: "KeyError" })).toBe(undefined);
		expect(await cell(hints, "b", [], { ename: "KeyError" })).toBe(undefined);
		expect(await cell(hints, "c", [], { ename: "KeyError" })).toMatch(
			/^\[hint:repeated-failure\] KeyError 3 cells in a row\. Read the traceback carefully/,
		);
		// A different error or a success resets the streak.
		expect(await cell(hints, "d", [], { ename: "ValueError" })).toBe(undefined);
		expect(await cell(hints, "e", [], { ename: "ValueError" })).toBe(undefined);
		expect(await cell(hints, "f")).toBe(undefined);
		expect(await cell(hints, "g", [], { ename: "ValueError" })).toBe(undefined);
	});

	test("one hint per cell, most specific first; a muted or spent tag gives way to the next", async () => {
		const hints = make({ maxPerTag: 1 });
		const detach: [string, Record<string, unknown>, unknown] = [
			"bash",
			{ yield_after: 30 },
			{ running: true, job: { id: "job-9" } },
		];
		const first = await cell(hints, "x", [detach], { text: TRUNCATED });
		expect(first).toMatch(/^\[hint:job-detached\]/);
		expect(first).not.toContain("output-truncated");
		// job-detached is spent (cap 1): the truncation hint fires instead.
		expect(await cell(hints, "x", [detach], { text: TRUNCATED })).toMatch(/^\[hint:output-truncated\]/);
		expect(await cell(hints, "x", [detach], { text: TRUNCATED })).toBe(undefined);
	});

	test("mute, unmute and muted are per lane and persist; each tag fires at most N times; ULTRON_HINTS=off", async () => {
		const hints = make();
		expect(await mute(hints, "hints.mute", ["output-truncated", "poll-loop"])).toEqual({
			muted: ["output-truncated", "poll-loop"],
		});
		expect(await cell(hints, "x", [], { text: TRUNCATED })).toBe(undefined);
		// Another lane (a subagent) is unaffected.
		expect(await cell(hints, "x", [], { text: TRUNCATED }, "ultron.rlm-child.t1")).toContain(
			"[hint:output-truncated]",
		);
		expect(await mute(hints, "hints.unmute", ["output-truncated"])).toEqual({ muted: ["poll-loop"] });
		expect(await mute(hints, "hints.muted", [])).toEqual({ muted: ["poll-loop"] });
		await expect(mute(hints, "hints.mute", ["no-such-tag"])).rejects.toThrow(/Unknown hint tag "no-such-tag"/);
		// The cap: three per tag per lane, then silence.
		const fired = [];
		for (let index = 0; index < 5; index += 1) fired.push(await cell(hints, "x", [], { text: TRUNCATED }));
		expect(fired.filter(Boolean)).toHaveLength(3);
		await hints.settled();
		// Mutes and counts are a session value: a restarted worker keeps them.
		const again = make();
		expect(await mute(again, "hints.muted", [])).toEqual({ muted: ["poll-loop"] });
		expect(await cell(again, "x", [], { text: TRUNCATED })).toBe(undefined);
		expect(await store.read()).toMatchObject({ version: 1, lanes: { main: { fired: { "output-truncated": 3 } } } });
		// Disabled: nothing fires.
		const off = make({ enabled: false, store: createMemoryModuleStore() });
		expect(await cell(off, "x", [], { text: TRUNCATED })).toBe(undefined);
		expect(hintsEnabled({ ULTRON_HINTS: "off" })).toBe(false);
		expect(hintsEnabled({})).toBe(true);
		expect(hintMaxPerTag({ ULTRON_HINTS_MAX: "1" })).toBe(1);
		expect(hintMaxPerTag({})).toBe(3);
		expect(readHandleBytes({})).toBe(256 * 1024);
	});
});

describe("kernel skills: hints and read", () => {
	let cwd: string;
	const kernels: RlmKernel[] = [];
	let hints: CellHints;

	/** A kernel whose `hints.*` go to the hints module and `rlm.load` stores content in a directory. */
	const kernel = (env: Record<string, string> = {}): RlmKernel => {
		const created = new RlmKernel({ cwd, runtimePath, env }, async (type, payload) => {
			if (type.startsWith("hints."))
				return hints.module.handle(
					{ type, payload, caller: { lane: "main" }, context: BACKGROUND_CONTEXT },
					{} as NativeHostApi,
				);
			if (type === "rlm.load")
				return { stored: false, path: join(cwd, ".context", String(payload.digest).replace(":", "-")) };
			throw new Error(`unexpected host request ${type}`);
		});
		kernels.push(created);
		return created;
	};

	beforeEach(() => {
		cwd = mkdtempSync(join(tmpdir(), "ultron-hints-"));
		hints = new CellHints({ store: createMemoryModuleStore() });
	});

	afterEach(async () => {
		for (const created of kernels.splice(0)) await created.shutdown();
		rmSync(cwd, { recursive: true, force: true });
	});

	test("hints.mute / unmute / muted from Python", async () => {
		const k = kernel();
		expect(await k.execute("await hints.mute('job-detached', 'poll-loop')")).toMatchObject({
			result: "['job-detached', 'poll-loop']",
		});
		expect(await k.execute("await hints.unmute('poll-loop')")).toMatchObject({ result: "['job-detached']" });
		expect(await k.execute("await hints.muted()")).toMatchObject({ result: "['job-detached']" });
		expect(await k.execute("await hints.mute()")).toMatchObject({
			status: "error",
			error: { ename: "TypeError" },
		});
	});

	test("read returns text up to the threshold and a ContextHandle with a note above it", async () => {
		writeFileSync(join(cwd, "small.txt"), "alpha\nbeta\n");
		writeFileSync(join(cwd, "big.log"), Array.from({ length: 400 }, (_, i) => `line ${i} ERROR maybe`).join("\n"));
		const k = kernel({ ULTRON_READ_HANDLE_BYTES: "4096" });
		expect(await k.execute("t = await read('small.txt'); (type(t).__name__, t)")).toMatchObject({
			result: "('str', 'alpha\\nbeta\\n')",
		});
		const big = await k.execute(
			"h = await read('big.log'); (type(h).__name__, h.line_count(), h.search('line 399')[0]['line'])",
		);
		expect(big).toMatchObject({ status: "ok", result: "('ContextHandle', 400, 399)" });
		expect(big.stdout).toMatch(
			/^\[read\] big\.log is 8,[0-9]{3} bytes \(over 4,096\), so it was loaded as a handle, not text: ContextHandle\(label='big\.log', chars=\d+, size=\d+, digest='sha256:[0-9a-f]{12}'…\)\. Use h\.search\(regex\), h\.lines\(a, b\), h\.chunks\(n\) or rlm\.map\(task, h\.chunks\(n\)\) on it\.\n$/,
		);
		// The content never reaches the output; the default threshold is 256 KiB.
		expect(big.stdout).not.toContain("line 12 ERROR");
		const plain = kernel();
		expect(await plain.execute("type(await read('big.log')).__name__")).toMatchObject({ result: "'str'" });
		expect(await plain.execute("await read('missing.txt')")).toMatchObject({
			status: "error",
			error: { ename: "FileNotFoundError" },
		});
	});
});
