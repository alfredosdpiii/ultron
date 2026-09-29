import { execFileSync } from "node:child_process";
import { chmod, mkdir, mkdtemp, rm, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { diffSnapshots, snapshotWorkspace } from "../src/ultron/workspace-snapshot.ts";

const directories: string[] = [];
afterEach(async () => {
	for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
});

async function temporary(): Promise<string> {
	const directory = await mkdtemp(join(tmpdir(), "ultron-snapshot-"));
	directories.push(directory);
	return directory;
}

function git(cwd: string, ...args: string[]): void {
	execFileSync("git", ["-c", "user.email=t@example.com", "-c", "user.name=t", ...args], { cwd, stdio: "ignore" });
}

async function write(root: string, path: string, text: string): Promise<void> {
	await mkdir(join(root, path, ".."), { recursive: true });
	await writeFile(join(root, path), text);
}

describe("workspace snapshots", () => {
	test("a Git working tree lists tracked and untracked files, not ignored ones, and sees every kind of change", async () => {
		const root = await temporary();
		git(root, "init", "-q");
		await write(root, ".gitignore", "build/\n*.log\n");
		await write(root, "src/a.ts", "a");
		await write(root, "src/b.ts", "b");
		await write(root, "docs/c.md", "c");
		git(root, "add", ".");
		git(root, "commit", "-q", "-m", "init");
		await write(root, "notes.txt", "untracked");
		await write(root, "build/out.js", "ignored");
		await write(root, "debug.log", "ignored");

		const before = await snapshotWorkspace(root);
		expect(before.method).toBe("git");
		expect(before.complete).toBe(true);
		expect([...before.entries.keys()].sort()).toEqual([
			".gitignore",
			"docs/c.md",
			"notes.txt",
			"src/a.ts",
			"src/b.ts",
		]);

		await write(root, "src/a.ts", "a changed");
		await unlink(join(root, "src/b.ts"));
		await write(root, "src/new.ts", "new");
		await chmod(join(root, "docs/c.md"), 0o600);
		await write(root, "build/out.js", "rebuilt");
		await write(root, "debug.log", "more");
		const after = await snapshotWorkspace(root);
		const diff = diffSnapshots(before, after);
		expect(diff).toEqual({
			added: ["src/new.ts"],
			modified: ["docs/c.md", "src/a.ts"],
			deleted: ["src/b.ts"],
			changed: ["docs/c.md", "src/a.ts", "src/b.ts", "src/new.ts"],
			complete: true,
		});
		// A deleted tracked file is still listed by Git but absent from the snapshot.
		expect(after.entries.has("src/b.ts")).toBe(false);
		expect(diffSnapshots(after, await snapshotWorkspace(root)).changed).toEqual([]);
	});

	test("a subdirectory of a repository is snapshotted relative to itself", async () => {
		const root = await temporary();
		git(root, "init", "-q");
		await write(root, "pkg/x.ts", "x");
		await write(root, "other/y.ts", "y");
		const snapshot = await snapshotWorkspace(join(root, "pkg"));
		expect(snapshot.method).toBe("git");
		expect([...snapshot.entries.keys()]).toEqual(["x.ts"]);
	});

	test("a directory outside Git is walked, skipping dependency and cache directories, without following links", async () => {
		const root = await temporary();
		await write(root, "main.py", "print(1)");
		await write(root, "pkg/util.py", "x = 1");
		await write(root, "node_modules/dep/index.js", "skip");
		await write(root, ".venv/lib/site.py", "skip");
		await write(root, "__pycache__/main.pyc", "skip");
		const outside = await temporary();
		await write(outside, "secret.txt", "outside");
		await symlink(outside, join(root, "linked"));
		const before = await snapshotWorkspace(root);
		expect(before.method).toBe("walk");
		expect([...before.entries.keys()].sort()).toEqual(["linked", "main.py", "pkg/util.py"]);
		expect(before.entries.get("linked")).toBe(`l:${outside}`);

		await write(root, "pkg/util.py", "x = 2");
		await write(root, "node_modules/dep/index.js", "changed, still skipped");
		await write(outside, "secret.txt", "changed outside");
		expect(diffSnapshots(before, await snapshotWorkspace(root)).changed).toEqual(["pkg/util.py"]);
	});

	test("the entry bound and exclusions are reported", async () => {
		const root = await temporary();
		for (let index = 0; index < 10; index += 1) await write(root, `f${index}.txt`, "x");
		const bounded = await snapshotWorkspace(root, { maxEntries: 4 });
		expect(bounded.complete).toBe(false);
		expect(bounded.entries.size).toBe(4);
		expect(diffSnapshots(bounded, await snapshotWorkspace(root)).complete).toBe(false);
		const excluded = await snapshotWorkspace(root, { exclude: (path) => path.startsWith("f1") });
		expect(excluded.entries.has("f1.txt")).toBe(false);
		expect(excluded.entries.size).toBe(9);
		const elsewhere = await snapshotWorkspace(await temporary());
		expect(() => diffSnapshots(excluded, elsewhere)).toThrow("different directories");
	});

	test("a few thousand files snapshot quickly, in Git and outside it", async () => {
		const root = await temporary();
		const writes: Promise<void>[] = [];
		for (let dir = 0; dir < 40; dir += 1)
			for (let file = 0; file < 100; file += 1) writes.push(write(root, `d${dir}/f${file}.txt`, `${dir}-${file}`));
		await Promise.all(writes);
		let started = Date.now();
		const walked = await snapshotWorkspace(root);
		const walkMs = Date.now() - started;
		expect(walked.entries.size).toBe(4000);
		git(root, "init", "-q");
		started = Date.now();
		const listed = await snapshotWorkspace(root);
		const gitMs = Date.now() - started;
		expect(listed.method).toBe("git");
		expect(listed.entries.size).toBe(4000);
		// Generous bounds for loaded CI machines; locally both take tens of milliseconds.
		expect(walkMs).toBeLessThan(3_000);
		expect(gitMs).toBeLessThan(3_000);
		await write(root, "d7/f7.txt", "changed");
		expect(diffSnapshots(listed, await snapshotWorkspace(root)).changed).toEqual(["d7/f7.txt"]);
	});
});
