import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, readlinkSync, realpathSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { FileHooks } from "../src/ultron/file-hooks.ts";
import { autoCommitSkipReason, autoInitEligibility } from "../src/ultron/loki.ts";
import {
	commitChildWorktree,
	commitMessage,
	conflictHunks,
	createChildWorktree,
	isUltronWorktreePath,
	listRecords,
	mergeChildWorktree,
	pruneWorktrees,
	removeChildWorktree,
	saveRecord,
	WorktreeError,
	type WorktreeRecord,
} from "../src/ultron/rlm/worktrees.ts";

/**
 * Sub-agent worktrees (worktrees.ts): a child's private checkout branches from its parent's tree without touching
 * it, its work is committed on its branch, and merging brings it back as uncommitted changes, or reports a conflict
 * and writes nothing. Crashed sessions' worktrees are pruned with their work kept on the branch.
 */

const directories: string[] = [];
afterEach(async () => {
	for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
});

function git(cwd: string, ...args: string[]): string {
	return execFileSync("git", args, { cwd, encoding: "utf8", env: { ...process.env, GIT_OPTIONAL_LOCKS: "0" } });
}

async function repository(files: Record<string, string> = {}): Promise<string> {
	const root = realpathSync(await mkdtemp(join(tmpdir(), "ultron-worktrees-")));
	directories.push(root);
	git(root, "init", "-q", "-b", "main");
	git(root, "config", "user.email", "test@example.com");
	git(root, "config", "user.name", "Test");
	git(root, "config", "commit.gpgsign", "false");
	const initial = {
		".gitignore": "node_modules/\n.venv/\n.env\ndist/\n",
		"src/app.py": "def one():\n    return 1\n\n\n\ndef two():\n    return 2\n\n\n\ndef three():\n    return 3\n",
		"README.md": "# demo\n",
		"old.txt": "to be deleted\n",
		...files,
	};
	for (const [path, content] of Object.entries(initial)) {
		await mkdir(join(root, path, ".."), { recursive: true });
		await writeFile(join(root, path), content);
	}
	git(root, "add", "-A");
	git(root, "commit", "-q", "-m", "initial");
	return root;
}

/** The parent's tree and index as Git sees them. */
function state(root: string): { status: string; index: string; head: string } {
	return {
		status: git(root, "status", "--porcelain=v1", "--untracked-files=all"),
		index: git(root, "ls-files", "-s"),
		head: git(root, "rev-parse", "HEAD").trim(),
	};
}

function create(root: string, name: string, extra: Partial<Parameters<typeof createChildWorktree>[0]> = {}) {
	return createChildWorktree({ cwd: root, sessionId: "sess1234abcd", name, ...extra });
}

describe("creating a worktree", () => {
	test("branches from the parent's dirty tree without touching its index or files", async () => {
		const root = await repository();
		await writeFile(join(root, "README.md"), "# demo\nstaged line\n");
		git(root, "add", "README.md");
		await writeFile(
			join(root, "src/app.py"),
			(await readFile(join(root, "src/app.py"), "utf8")).replace("return 1", "return 10"),
		);
		await writeFile(join(root, "notes.txt"), "untracked\n");
		await rm(join(root, "old.txt"));
		const before = state(root);
		const record = await create(root, "Fix Parser!");
		expect(state(root)).toEqual(before);
		expect(record.branch).toBe("ultron/sess1234/fix-parser");
		expect(
			record.path.startsWith(
				join(git(root, "rev-parse", "--path-format=absolute", "--git-common-dir").trim(), "ultron-worktrees"),
			),
		).toBe(true);
		expect(record.dirty).toBe(true);
		expect(record.head).toBe(before.head);
		// The child sees the parent's staged, unstaged and untracked changes and its deletions.
		expect(await readFile(join(record.path, "README.md"), "utf8")).toContain("staged line");
		expect(await readFile(join(record.path, "src/app.py"), "utf8")).toContain("return 10");
		expect(await readFile(join(record.path, "notes.txt"), "utf8")).toBe("untracked\n");
		expect(existsSync(join(record.path, "old.txt"))).toBe(false);
		expect(git(record.path, "status", "--porcelain")).toBe("");
		// The parent's own status never lists the worktree.
		expect(git(root, "status", "--porcelain=v1", "--untracked-files=all")).not.toContain("ultron-worktrees");
		expect((await listRecords(root)).map((item) => item.id)).toEqual([record.id]);
	});

	test("a clean parent branches at HEAD; names are never reused", async () => {
		const root = await repository();
		const first = await create(root, "worker");
		const second = await create(root, "worker");
		expect(first.base).toBe(git(root, "rev-parse", "HEAD").trim());
		expect(first.dirty).toBe(false);
		expect(second.branch).toBe("ultron/sess1234/worker-2");
		expect(second.path).not.toBe(first.path);
	});

	test("children spawned at the same time with the same name get their own branches", async () => {
		const root = await repository();
		const records = await Promise.all([create(root, "same"), create(root, "same"), create(root, "same")]);
		expect(records.map((record) => record.branch).sort()).toEqual([
			"ultron/sess1234/same",
			"ultron/sess1234/same-2",
			"ultron/sess1234/same-3",
		]);
	});

	test("a subdirectory working directory maps to the same subdirectory", async () => {
		const root = await repository();
		const record = await create(join(root, "src"), "sub");
		expect(record.cwd).toBe(join(record.path, "src"));
		expect(record.repo).toBe(root);
	});

	test("outside Git, without Git commits: a clear error that allows the shared-tree fallback", async () => {
		const plain = realpathSync(await mkdtemp(join(tmpdir(), "ultron-worktrees-plain-")));
		directories.push(plain);
		const error = await create(plain, "x").catch((caught: unknown) => caught);
		expect(error).toBeInstanceOf(WorktreeError);
		expect((error as WorktreeError).message).toMatch(/needs a Git repository.*not inside one/);
		expect((error as WorktreeError).fallback).toBe(true);
		git(plain, "init", "-q");
		const empty = await create(plain, "x").catch((caught: unknown) => caught);
		expect((empty as WorktreeError).message).toMatch(/has none yet/);
		expect((empty as WorktreeError).fallback).toBe(true);
	});

	test("links gitignored dependencies (workspace packages to the worktree's own) and copies .env", async () => {
		const root = await repository({ "packages/lib/index.js": "module.exports = 1;\n" });
		await mkdir(join(root, "node_modules", "left-pad"), { recursive: true });
		await writeFile(join(root, "node_modules", "left-pad", "index.js"), "pad");
		await mkdir(join(root, "node_modules", "@scope"), { recursive: true });
		await symlink("../../packages/lib", join(root, "node_modules", "@scope", "lib"));
		await mkdir(join(root, ".venv", "bin"), { recursive: true });
		await writeFile(join(root, ".venv", "bin", "python"), "#!/bin/sh\n");
		await writeFile(join(root, ".env"), "TOKEN=local\n");
		await mkdir(join(root, "dist"), { recursive: true });
		await writeFile(join(root, "dist", "out.js"), "built");
		const record = await create(root, "deps");
		expect(record.linked.sort()).toEqual([".venv", "node_modules"]);
		expect(record.copied).toEqual([".env"]);
		expect(readlinkSync(join(record.path, "node_modules", "left-pad"))).toBe(join(root, "node_modules", "left-pad"));
		// A workspace package resolves to the worktree's sources, not the parent's.
		expect(readlinkSync(join(record.path, "node_modules", "@scope", "lib"))).toBe(join(record.path, "packages/lib"));
		expect(readlinkSync(join(record.path, ".venv", "bin"))).toBe(join(root, ".venv", "bin"));
		expect(await readFile(join(record.path, ".env"), "utf8")).toBe("TOKEN=local\n");
		// Build output is not linked (children rebuild it); links never show as changes.
		expect(existsSync(join(record.path, "dist"))).toBe(false);
		expect(git(record.path, "status", "--porcelain", "--untracked-files=all")).toBe("");
		await writeFile(join(record.path, "src/app.py"), "changed\n");
		const commit = await commitChildWorktree(record, { message: "change" });
		expect(commit.changedFiles).toEqual(["src/app.py"]);
		await removeChildWorktree(record, { deleteBranch: true });
		// Removing the worktree removes the links, never what they point to.
		expect(await readFile(join(root, "node_modules", "left-pad", "index.js"), "utf8")).toBe("pad");
		expect(existsSync(join(root, ".venv", "bin", "python"))).toBe(true);
		expect(existsSync(record.path)).toBe(false);
	});

	test("explicit setup lists replace the defaults and must exist", async () => {
		const root = await repository();
		await mkdir(join(root, "node_modules", "a"), { recursive: true });
		const none = await create(root, "none", { setup: { link: [], copy: [] } });
		expect(none.linked).toEqual([]);
		await expect(create(root, "bad", { setup: { link: ["missing"] } })).rejects.toThrow(/does not exist/);
		await expect(create(root, "escape", { setup: { link: ["../x"] } })).rejects.toThrow(/must be relative/);
		expect(git(root, "branch", "--list", "ultron/*")).not.toContain("bad");
	});
});

describe("committing and merging", () => {
	test("a child's edits stay in its worktree until merged, then land uncommitted", async () => {
		const root = await repository();
		const record = await create(root, "edit");
		const app = join(record.path, "src/app.py");
		await writeFile(app, (await readFile(app, "utf8")).replace("return 2", "return 20"));
		await writeFile(join(record.path, "new.py"), "print('new')\n");
		await rm(join(record.path, "old.txt"));
		const before = state(root);
		expect(await readFile(join(root, "src/app.py"), "utf8")).toContain("return 2\n");
		const commit = await commitChildWorktree(record, {
			message: commitMessage("Return twenty from two()", "edit", "task-1", "passed"),
		});
		expect(commit.commit).toMatch(/^[0-9a-f]{40}$/);
		expect(commit.changedFiles).toEqual(["new.py", "old.txt", "src/app.py"]);
		expect(commit.diffstat).toMatch(/3 files changed/);
		const message = git(root, "log", "-1", "--format=%B", commit.commit!);
		expect(message).toContain("Return twenty from two()");
		expect(message).toContain('Ultron sub-agent "edit" (task task-1), passed.');
		expect(message).not.toMatch(/Co-Authored-By|Generated with/i);
		expect(state(root)).toEqual(before);
		const merged = await mergeChildWorktree(root, {
			name: "edit",
			branch: record.branch,
			base: record.base,
			commit: commit.commit,
		});
		expect(merged).toMatchObject({ status: "merged", files: ["new.py", "old.txt", "src/app.py"] });
		expect(await readFile(join(root, "src/app.py"), "utf8")).toContain("return 20");
		expect(existsSync(join(root, "old.txt"))).toBe(false);
		// Uncommitted: HEAD and the index are as they were; the new file is untracked.
		const after = state(root);
		expect(after.head).toBe(before.head);
		expect(after.index).toBe(before.index);
		expect(after.status).toContain(" M src/app.py");
		expect(after.status).toContain(" D old.txt");
		expect(after.status).toContain("?? new.py");
	});

	test("merges on top of the parent's own later edits (three-way)", async () => {
		const root = await repository();
		const record = await create(root, "three");
		const app = join(record.path, "src/app.py");
		await writeFile(app, (await readFile(app, "utf8")).replace("return 3", "return 30"));
		const commit = await commitChildWorktree(record, { message: "three" });
		// Meanwhile the parent edits another function of the same file.
		await writeFile(
			join(root, "src/app.py"),
			(await readFile(join(root, "src/app.py"), "utf8")).replace("return 1", "return 100"),
		);
		const merged = await mergeChildWorktree(root, {
			name: "three",
			branch: record.branch,
			base: record.base,
			commit: commit.commit,
		});
		expect(merged.status).toBe("merged");
		const text = await readFile(join(root, "src/app.py"), "utf8");
		expect(text).toContain("return 100");
		expect(text).toContain("return 30");
	});

	test("a conflict writes nothing and names the files and hunks; markers only when asked", async () => {
		const root = await repository();
		const a = await create(root, "a");
		const b = await create(root, "b");
		await writeFile(
			join(a.path, "src/app.py"),
			(await readFile(join(a.path, "src/app.py"), "utf8")).replace("return 2", "return 'a'"),
		);
		await writeFile(
			join(b.path, "src/app.py"),
			(await readFile(join(b.path, "src/app.py"), "utf8")).replace("return 2", "return 'b'"),
		);
		await writeFile(join(b.path, "b-only.txt"), "b\n");
		const ca = await commitChildWorktree(a, { message: "a" });
		const cb = await commitChildWorktree(b, { message: "b" });
		expect(
			(await mergeChildWorktree(root, { name: "a", branch: a.branch, base: a.base, commit: ca.commit })).status,
		).toBe("merged");
		const afterA = state(root);
		const textA = await readFile(join(root, "src/app.py"), "utf8");
		const conflict = await mergeChildWorktree(root, { name: "b", branch: b.branch, base: b.base, commit: cb.commit });
		expect(conflict).toMatchObject({ status: "conflict", files: ["src/app.py"], markers: false });
		if (conflict.status !== "conflict") throw new Error("expected a conflict");
		expect(conflict.conflicts[0]).toMatchObject({
			path: "src/app.py",
			kind: expect.stringContaining("CONFLICT"),
			hunks: [{ line: expect.any(Number), ours: "    return 'a'", theirs: "    return 'b'" }],
		});
		// Nothing of b was written: not even its non-conflicting new file.
		expect(state(root)).toEqual(afterA);
		expect(await readFile(join(root, "src/app.py"), "utf8")).toBe(textA);
		expect(existsSync(join(root, "b-only.txt"))).toBe(false);
		const marked = await mergeChildWorktree(
			root,
			{ name: "b", branch: b.branch, base: b.base, commit: cb.commit },
			{ onConflict: "markers" },
		);
		expect(marked).toMatchObject({ status: "conflict", markers: true });
		const text = await readFile(join(root, "src/app.py"), "utf8");
		expect(text).toContain("<<<<<<< working tree");
		expect(text).toContain(`>>>>>>> ${b.branch}`);
		expect(existsSync(join(root, "b-only.txt"))).toBe(true);
	});

	test("an unchanged child is empty; conflict hunks parse diff3 style too", async () => {
		const root = await repository();
		const record = await create(root, "idle");
		expect(await commitChildWorktree(record, { message: "none" })).toEqual({
			commit: null,
			changedFiles: [],
			diffstat: "",
		});
		expect(
			await mergeChildWorktree(root, { name: "idle", branch: record.branch, base: record.base, commit: null }),
		).toEqual({
			status: "empty",
		});
		expect(conflictHunks("a\n<<<<<<< x\nours\n||||||| base\nold\n=======\ntheirs\n>>>>>>> y\n")).toEqual([
			{ line: 2, ours: "ours", theirs: "theirs" },
		]);
	});
});

describe("cleanup and crashes", () => {
	/** A pid that no longer runs. */
	function deadPid(): number {
		const child = spawnSync(process.execPath, ["-e", "process.stdout.write(String(process.pid))"], {
			encoding: "utf8",
		});
		return Number(child.stdout);
	}

	async function orphan(record: WorktreeRecord): Promise<void> {
		await saveRecord({ ...record, owner: { ...record.owner, pid: deadPid() } } as WorktreeRecord);
	}

	test("a crashed session's worktrees are pruned; uncommitted work is kept on the branch", async () => {
		const root = await repository();
		const working = await create(root, "working");
		const idle = await create(root, "idle");
		const alive = await create(root, "alive");
		await writeFile(join(working.path, "half-done.txt"), "work in progress\n");
		await orphan(working);
		await orphan(idle);
		const report = await pruneWorktrees(root);
		expect(report.removed.sort()).toEqual([idle.path, working.path].sort());
		expect(report.keptBranches).toEqual([working.branch]);
		expect(existsSync(working.path)).toBe(false);
		expect(existsSync(idle.path)).toBe(false);
		expect(existsSync(alive.path)).toBe(true);
		// The interrupted work is a commit on its branch; the idle branch is gone.
		expect(git(root, "show", `${working.branch}:half-done.txt`)).toBe("work in progress\n");
		expect(git(root, "log", "-1", "--format=%s", working.branch)).toContain("Uncommitted work of sub-agent working");
		expect(git(root, "branch", "--list", idle.branch)).toBe("");
		expect(git(root, "worktree", "list")).not.toContain(working.path);
		expect((await listRecords(root)).map((item) => item.id)).toEqual([alive.id]);
	});

	test("a torn record's directory is removed too, never anything outside Ultron's own", async () => {
		const root = await repository();
		const record = await create(root, "torn");
		await rm(join(record.commonDir, "ultron-worktrees", `${record.id}.json`));
		await pruneWorktrees(root);
		expect(existsSync(record.path)).toBe(false);
		expect(existsSync(join(root, "src/app.py"))).toBe(true);
	});
});

describe("Loki in worktrees", () => {
	test("never auto-installs or auto-commits .loki/ in a sub-agent worktree", async () => {
		const root = await repository();
		const record = await create(root, "loki");
		const options = { env: {} as NodeJS.ProcessEnv, settings: {}, home: "/nonexistent-home" };
		expect(await autoInitEligibility(root, options)).toEqual({ root });
		expect(await autoInitEligibility(record.path, options)).toEqual({
			reason: "this is an Ultron sub-agent worktree",
		});
		expect(await autoCommitSkipReason(record.path, options)).toBe("this is an Ultron sub-agent worktree");
		// A child server started in a worktree knows by its environment, wherever the path is.
		expect(isUltronWorktreePath("/anywhere", { ULTRON_WORKTREE: record.path })).toBe(true);
		expect(isUltronWorktreePath(root, {})).toBe(false);
	});

	test("file hooks resolve and check a worktree lane's writes in its worktree", async () => {
		const root = await repository();
		const record = await create(root, "hooks");
		const seen: Array<{ path: string; cwd: string; root?: string }> = [];
		const hooks = new FileHooks({
			cwd: root,
			laneRoot: (lane) => (lane === "child" ? { cwd: record.cwd, root: record.path } : undefined),
		});
		hooks.add({
			name: "Probe",
			beforeWrite: async (write, context) => {
				seen.push({
					path: write.path,
					cwd: context.cwd,
					...(context.root === undefined ? {} : { root: context.root }),
				});
				return {};
			},
		});
		await hooks.beforeWrite([{ path: "src/app.py", content: "x" }], { lane: "child" });
		await hooks.beforeWrite([{ path: "src/app.py", content: "x" }], { lane: "main" });
		expect(seen).toEqual([
			{ path: join(record.path, "src/app.py"), cwd: record.cwd, root: record.path },
			{ path: join(root, "src/app.py"), cwd: root },
		]);
	});
});
