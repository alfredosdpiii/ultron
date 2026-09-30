import { execFileSync } from "node:child_process";
import { realpathSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BACKGROUND_CONTEXT } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { afterEach, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { createChildWorktree, worktreeEnv } from "../src/ultron/rlm/worktrees.ts";

/**
 * A worktree subagent's kernel runs in its worktree with ULTRON_WORKTREE set, and read/edit/write map absolute paths
 * into the parent's checkout (as briefs often give them) to the worktree's copies, saying so once.
 */

const directories: string[] = [];
afterEach(async () => {
	for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
});

test("a worktree lane's kernel works in its worktree and maps the parent's absolute paths into it", async () => {
	const root = realpathSync(await mkdtemp(join(tmpdir(), "ultron-worktree-kernel-")));
	directories.push(root);
	const git = (...args: string[]) => execFileSync("git", args, { cwd: root, encoding: "utf8" });
	git("init", "-q", "-b", "main");
	git("config", "user.email", "test@example.com");
	git("config", "user.name", "Test");
	await writeFile(join(root, "app.py"), "VALUE = 1\n");
	git("add", "-A");
	git("commit", "-q", "-m", "initial");
	const record = await createChildWorktree({ cwd: root, sessionId: "kernel", name: "child" });
	const tool = createUltronRlmTool(
		root,
		async (type) => {
			throw new Error(`unexpected host request ${type}`);
		},
		async (invocation) => (invocation.operationId === "child-op" ? "child" : "main"),
		{ laneWorkspace: (lane) => (lane === "child" ? { cwd: record.cwd, env: worktreeEnv(record) } : undefined) },
	);
	const run = async (operationId: string, code: string): Promise<string> => {
		const result = await tool.execute(
			"call",
			{ code },
			() => {},
			{ env: new NodeExecutionEnv({ cwd: root }) },
			{
				invocationId: operationId,
				operationId,
				turnId: "turn",
				getMemo: async () => undefined,
				setMemo: async () => undefined,
			},
			BACKGROUND_CONTEXT,
		);
		return (result.content[0] as { text: string }).text;
	};
	try {
		expect(await run("child-op", "import os\nprint(os.getcwd()); print(os.environ['ULTRON_WORKTREE'])")).toBe(
			`${record.cwd}\n${record.path}`,
		);
		expect(await run("main-op", "import os\nos.getcwd()")).toContain(root);
		const parentPath = join(root, "app.py");
		const edited = await run("child-op", `await edit(${JSON.stringify(parentPath)}, "VALUE = 1", "VALUE = 2")`);
		expect(edited).toContain(`[worktree] ${parentPath} is in your parent's checkout; using your worktree's copy`);
		expect(await readFile(join(record.path, "app.py"), "utf8")).toBe("VALUE = 2\n");
		expect(await readFile(parentPath, "utf8")).toBe("VALUE = 1\n");
		// Said once per path; reads and writes map too.
		const read = await run("child-op", `await read(${JSON.stringify(parentPath)})`);
		expect(read).toContain("VALUE = 2");
		expect(read).not.toContain("[worktree]");
		await run("child-op", `await write(${JSON.stringify(join(root, "new.py"))}, "NEW = 1\\n")`);
		expect(await readFile(join(record.path, "new.py"), "utf8")).toBe("NEW = 1\n");
		await expect(readFile(join(root, "new.py"), "utf8")).rejects.toThrow();
		// The root lane's absolute paths are its own.
		await run("main-op", `await write(${JSON.stringify(join(root, "main.py"))}, "MAIN = 1\\n")`);
		expect(await readFile(join(root, "main.py"), "utf8")).toBe("MAIN = 1\n");
	} finally {
		await tool.close();
	}
});
