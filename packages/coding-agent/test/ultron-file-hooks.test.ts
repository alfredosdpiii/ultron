/**
 * Generic file hooks in the RLM kernel (file-hooks.ts):
 * - `edit()` and `write()` show the proposed content to the before-write guards; a guard that blocks makes the call
 *   raise ValueError with its reason and nothing is written;
 * - a guard that does not answer in time lets the write proceed with a visible "did not check" note;
 * - files a cell changes by other means (`bash('sed -i ...')`) reach the after-cell guards in the background, and
 *   their findings arrive with the lane's next cell result; checked writes are reported as `checked`, not as `files`;
 * - a `bash()` command that may write a file goes to the guards' `beforeShell` first: one that blocks refuses the
 *   command before it runs; commands that write no file never reach a guard.
 */
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createLocalBashOperations } from "../src/core/tools/bash.ts";
import { createUltronRlmTool, type UltronRlmTool } from "../src/experimental/session-worker.ts";
import {
	type BeforeWriteVerdict,
	type CellChanges,
	FileHooks,
	mayWriteFiles,
	type ProposedWrite,
} from "../src/ultron/file-hooks.ts";
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

	test("a shell command that writes a file is refused before it runs when a guard blocks it", async () => {
		const seen: Array<{ command: string; cwd: string }> = [];
		hooks.add({
			name: "Fake",
			beforeShell: async (command, context): Promise<BeforeWriteVerdict> => {
				seen.push({ command, cwd: context.cwd });
				return command.includes("app.py") ? { block: true, reason: "loki: `redirect` into app.py" } : {};
			},
		});
		const refused = await run("await bash('echo VALUE = 2 > app.py')");
		expect(refused).toContain("bash command was not run: [Fake] loki: `redirect` into app.py");
		expect(readFileSync(join(dir, "app.py"), "utf8")).toBe("VALUE = 1\n");

		const notes = await run("print(await bash('echo beta >> notes.txt && cat notes.txt'))");
		expect(notes).toContain("alpha\nbeta");
		// A command that writes no file never reaches the guard.
		await run("print(await bash('grep -c VALUE app.py 2>/dev/null | tail -1'))");
		expect(seen).toEqual([
			{ command: "echo VALUE = 2 > app.py", cwd: dir },
			{ command: "echo beta >> notes.txt && cat notes.txt", cwd: dir },
		]);
		expect(hooks.stats()[0]).toMatchObject({ checks: 2, blocked: 1 });
	});

	test("a shell guard that does not answer in time lets the command run", async () => {
		hooks.add({ name: "Slow", timeoutMs: 200, beforeShell: () => new Promise(() => {}) });
		await run("await bash('echo VALUE = 3 > app.py')");
		expect(readFileSync(join(dir, "app.py"), "utf8")).toBe("VALUE = 3\n");
		expect(hooks.stats()[0]).toMatchObject({ checks: 1, unchecked: 1 });
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

describe("mayWriteFiles", () => {
	test("finds the shell forms that write a file and passes over the rest", () => {
		for (const command of [
			"echo x > a.py",
			"cat >> log.txt",
			"cat > out.py <<'EOF'\nx\nEOF",
			"printf x | tee a.py",
			"sed -i 's/a/b/' a.py",
			"perl -pi -e 's/a/b/' a.py",
			"cp a b",
			"mv a b",
			"install -m 644 a b",
			"dd if=/dev/zero of=z bs=1",
			"make &> build.log",
		])
			expect(mayWriteFiles(command), command).toBe(true);
		for (const command of [
			"pytest -q 2>&1 | tail -5",
			"rg -n foo src > /dev/null",
			"grep x a 2>/dev/null",
			"sed -n 1p a.py",
			"git log --oneline -3",
			"ls -la",
		])
			expect(mayWriteFiles(command), command).toBe(false);
	});
});
