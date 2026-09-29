/**
 * Generic file hooks in the RLM kernel (file-hooks.ts):
 * - `edit()` and `write()` show the proposed content to the before-write guards; a guard that blocks makes the call
 *   raise ValueError with its reason and nothing is written;
 * - a guard that does not answer in time lets the write proceed with a visible "did not check" note;
 * - files a cell changes by other means (`bash('sed -i ...')`) reach the after-cell guards in the background, and
 *   their findings arrive with the lane's next cell result; checked writes are reported as `checked`, not as `files`.
 */
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createLocalBashOperations } from "../src/core/tools/bash.ts";
import { createUltronRlmTool, type UltronRlmTool } from "../src/experimental/session-worker.ts";
import { type CellChanges, FileHooks, type ProposedWrite } from "../src/ultron/file-hooks.ts";
import { runHostBash } from "../src/ultron/rlm/host-bash.ts";

describe("file hooks in the RLM kernel", () => {
	let dir: string;
	let tool: UltronRlmTool;
	let hooks: FileHooks;
	let call = 0;
	const run = async (code: string): Promise<string> => {
		call += 1;
		const invocation = {
			invocationId: `hooks-${call}`,
			operationId: `hooks-op-${call}`,
			turnId: "hooks-turn",
			getMemo: async () => undefined,
			setMemo: async () => undefined,
		};
		try {
			const result = (await tool.execute(
				`hooks-${call}`,
				{ code },
				() => {},
				{ env: new NodeExecutionEnv({ cwd: dir }) },
				invocation,
				BACKGROUND_CONTEXT,
			)) as { content: Array<{ text: string }> };
			return result.content.map((part) => part.text).join("");
		} catch (error) {
			return `ERROR ${error instanceof Error ? error.message : String(error)}`;
		}
	};

	beforeEach(() => {
		dir = mkdtempSync(join(tmpdir(), "ultron-file-hooks-"));
		writeFileSync(join(dir, "app.py"), "VALUE = 1\n");
		writeFileSync(join(dir, "notes.txt"), "alpha\n");
		hooks = new FileHooks({ cwd: dir });
		tool = createUltronRlmTool(
			dir,
			async (type, payload, signal) => {
				if (type === "bash") return runHostBash(payload, dir, createLocalBashOperations({}), signal);
				throw new Error(`unexpected host request ${type}`);
			},
			async () => "main",
			{ fileHooks: hooks },
		);
	});

	afterEach(async () => {
		await tool.close();
		rmSync(dir, { recursive: true, force: true });
	});

	test("a blocking guard makes edit() and write() raise ValueError and write nothing", async () => {
		const seen: ProposedWrite[] = [];
		hooks.add({
			name: "Fake",
			beforeWrite: async (write) => {
				seen.push(write);
				return write.content.includes("forbidden")
					? { block: true, reason: "app.py:1: fake/rule: forbidden text" }
					: { message: "looks fine" };
			},
		});
		const blocked = await run(`await edit('app.py', 'VALUE = 1', 'VALUE = "forbidden"')`);
		expect(blocked).toContain("ValueError: app.py was not written: [Fake] app.py:1: fake/rule: forbidden text");
		expect(readFileSync(join(dir, "app.py"), "utf8")).toBe("VALUE = 1\n");
		expect(seen[0]).toEqual({ path: join(dir, "app.py"), content: 'VALUE = "forbidden"\n' });

		const created = await run("await write('pkg/new.py', 'forbidden = True\\n')");
		expect(created).toContain("ValueError: pkg/new.py was not written");
		expect(existsSync(join(dir, "pkg"))).toBe(false);

		const allowed = await run("await edit('app.py', 'VALUE = 1', 'VALUE = 2')");
		expect(allowed).toContain("[Fake] looks fine");
		expect(allowed).toContain("Edited app.py");
		expect(readFileSync(join(dir, "app.py"), "utf8")).toBe("VALUE = 2\n");
		// Several writes in one cell are checked in parallel.
		const both = await run(
			"await asyncio.gather(write('a.txt', 'one'), write('b.txt', 'two')); print(open('a.txt').read(), open('b.txt').read())",
		);
		expect(both).toContain("one two");
		expect(hooks.stats()[0]).toMatchObject({ name: "Fake", checks: 5, blocked: 2, unchecked: 0 });
	});

	test("a guard that does not answer in time lets the write proceed with a visible note", async () => {
		hooks.add({ name: "Slow", timeoutMs: 200, beforeWrite: () => new Promise(() => {}) });
		hooks.add({
			name: "Broken",
			beforeWrite: async () => {
				throw new Error("checker crashed");
			},
		});
		const output = await run("await write('out.txt', 'hello')");
		expect(output).toContain(
			"[Slow] Slow did not check this write to out.txt (no answer within 0.2 s); it was written unchecked.",
		);
		expect(output).toContain(
			"[Broken] Broken did not check this write to out.txt (checker crashed); it was written unchecked.",
		);
		expect(readFileSync(join(dir, "out.txt"), "utf8")).toBe("hello");
		expect(hooks.stats().map((stats) => stats.unchecked)).toEqual([1, 1]);
	});

	test("after a cell, files changed through bash reach the guards and their findings come with the next cell", async () => {
		const reports: CellChanges[] = [];
		hooks.add({
			name: "Watch",
			beforeWrite: async () => ({}),
			afterCellChanges: async (changes) => {
				reports.push(changes);
				return changes.files.length > 0
					? `unchecked writes: ${changes.files.map((path) => path.slice(dir.length + 1)).join(", ")}`
					: undefined;
			},
		});
		hooks.start();
		const first = await run(
			"await bash('''sed -i 's/alpha/beta/' notes.txt''')\nawait edit('app.py', 'VALUE = 1', 'VALUE = 3')\nprint('done')",
		);
		expect(first).toContain("done");
		expect(readFileSync(join(dir, "notes.txt"), "utf8")).toBe("beta\n");
		await hooks.settled();
		expect(reports).toHaveLength(1);
		expect(reports[0]!.files).toEqual([join(dir, "notes.txt")]);
		expect(reports[0]!.checked).toEqual([join(dir, "app.py")]);
		const next = await run("print('next')");
		expect(next).toContain("next\n[Watch] unchecked writes: notes.txt");
		await hooks.settled();
		// Nothing changed in the second cell, so nothing more is reported.
		expect(reports).toHaveLength(1);
		const later = await run("print('later')");
		expect(later).not.toContain("[Watch]");
	});
});
