import { execFileSync } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Context } from "@ultron/agent-core";
import { afterEach, describe, expect, test } from "vitest";
import { runClaudeChild } from "../src/ultron/claude/child.ts";
import type { NativeExternalChildRun } from "../src/ultron/rlm/native-host.ts";
import { verdictTag } from "../src/ultron/rlm/verdict.ts";
import { listRecords } from "../src/ultron/rlm/worktrees.ts";
import { hostFixture } from "./ultron-host-fixtures.ts";

/**
 * `rlm.spawn(brief, name=..., worktree=True)` on the host, with scripted children: each child works in its own
 * worktree (its lane's workspace), its work is committed when it ends and reported in its result, and `rlm.merge`
 * brings the children's work into the parent's tree in order, stopping at the first conflict with the tree as it was.
 */

const directories: string[] = [];
const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
	for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
});

function git(cwd: string, ...args: string[]): string {
	return execFileSync("git", args, { cwd, encoding: "utf8" });
}

const APP = [
	"def one():",
	"    return 1",
	"",
	"",
	"",
	"def two():",
	"    return 2",
	"",
	"",
	"",
	"def three():",
	"    return 3",
	"",
].join("\n");

async function repository(): Promise<string> {
	const root = realpathSync(await mkdtemp(join(tmpdir(), "ultron-worktree-host-")));
	directories.push(root);
	git(root, "init", "-q", "-b", "main");
	git(root, "config", "user.email", "test@example.com");
	git(root, "config", "user.name", "Test");
	await writeFile(join(root, ".gitignore"), "node_modules/\n");
	await writeFile(join(root, "app.py"), APP);
	git(root, "add", "-A");
	git(root, "commit", "-q", "-m", "initial");
	return root;
}

type Tools = {
	/** The directory this child works in (its worktree, or the shared tree). */
	cwd: string;
	lane: string;
	context: Context;
	finish: (payload: Record<string, unknown>) => Promise<unknown>;
	/** A host request as this child's lane. */
	call: <T = unknown>(type: string, payload?: Record<string, unknown>) => Promise<T>;
	edit: (path: string, from: string, to: string) => Promise<void>;
};
type Script = (tools: Tools) => Promise<string>;

type ChildResult = {
	status: string;
	value?: string;
	error?: string;
	verdict: { status: string; changed_files: string[] } | null;
	check?: { outcome: string; unreported: string[]; unobserved: string[]; outside?: string[] };
	worktree?: {
		task: string;
		branch: string;
		path: string;
		base: string;
		commit: string | null;
		changed_files: string[];
		diffstat: string;
		removed?: true;
	};
};

function worktreeHost(workspace: string, scripts: Record<string, Script>) {
	const fixture = hostFixture({
		workspace,
		script: async (lane, prompt, context) => {
			// The brief is followed by the worktree note; the first line names the script.
			const key = prompt.split("\n")[0]!;
			const script = scripts[key];
			if (!script) throw new Error(`no script for ${key}`);
			const cwd = fixture.host.laneWorkspace(lane)?.cwd ?? workspace;
			const call = <T>(type: string, payload: Record<string, unknown> = {}) =>
				fixture.host.handle(type, payload, context, { lane }) as Promise<T>;
			return script({
				cwd,
				lane,
				context,
				call,
				finish: (payload) => call("rlm.finish", payload),
				edit: async (path, from, to) => {
					const file = join(cwd, path);
					const text = await readFile(file, "utf8");
					if (!text.includes(from)) throw new Error(`${from} not in ${path}`);
					await writeFile(file, text.replace(from, to));
				},
			});
		},
	});
	fixtures.push(fixture);
	type Handle = { rlm_child_id: string; worktree: { branch: string; path: string; cwd: string } | null };
	const spawn = (name: string, kwargs: Record<string, unknown> = {}) =>
		fixture.call<Handle>("rlm.spawn", { prompt: name, kwargs: { name, worktree: true, ...kwargs } });
	const collect = async (ids: string[]) =>
		(
			await fixture.call<{ results: Array<{ id: string; result: ChildResult }> }>("rlm.collect", { selectors: ids })
		).results.map((item) => item.result);
	const merge = (ids: string[], options: Record<string, unknown> = {}) =>
		fixture.call<{
			ok: boolean;
			merged: string[];
			stopped_at?: string;
			results: Array<Record<string, unknown> & { id: string; status: string }>;
		}>("rlm.merge", { selectors: ids, ...options });
	return { fixture, spawn, collect, merge };
}

const passed = (files: string[]) => ({
	status: "passed",
	summary: `Changed ${files.join(", ")}`,
	evidence: files.map((file) => `${file}:1 changed`),
	changed_files: files,
});

describe("worktree children on the host", () => {
	test("children edit the same file in isolation; the parent sees nothing until it merges, then uncommitted work", async () => {
		const root = await repository();
		// The parent has uncommitted work of its own when it spawns: children start from it.
		await writeFile(join(root, "notes.md"), "parent's notes\n");
		const { spawn, collect, merge } = worktreeHost(root, {
			one: async ({ cwd, edit, finish }) => {
				expect(await readFile(join(cwd, "notes.md"), "utf8")).toBe("parent's notes\n");
				await edit("app.py", "return 1", "return 'one'");
				await finish(passed(["app.py"]));
				return "one done";
			},
			three: async ({ edit, finish, cwd }) => {
				await edit("app.py", "return 3", "return 'three'");
				await writeFile(join(cwd, "three.py"), "THREE = 3\n");
				await finish(passed(["app.py", "three.py"]));
				return "three done";
			},
		});
		const statusBefore = git(root, "status", "--porcelain");
		const indexBefore = git(root, "ls-files", "-s");
		const one = await spawn("one");
		const three = await spawn("three");
		expect(one.worktree?.branch).toBe("ultron/session/one");
		expect(one.worktree?.path).not.toBe(three.worktree?.path);
		const [first, second] = await collect([one.rlm_child_id, three.rlm_child_id]);
		for (const result of [first!, second!]) {
			expect(result.status).toBe("succeeded");
			// Each child's check sees only its own worktree: no sibling noise.
			expect(result.check).toMatchObject({ outcome: "verified", unreported: [], unobserved: [] });
			expect(result.check?.outside).toBeUndefined();
			expect(result.worktree?.commit).toMatch(/^[0-9a-f]{40}$/);
		}
		expect(first!.worktree).toMatchObject({
			branch: "ultron/session/one",
			changed_files: ["app.py"],
			task: one.rlm_child_id,
		});
		expect(second!.worktree?.changed_files).toEqual(["app.py", "three.py"]);
		expect(second!.worktree?.diffstat).toMatch(/2 files changed/);
		expect(git(root, "log", "-1", "--format=%s", second!.worktree!.commit!).trim()).toBe("Changed app.py, three.py");
		// Until the merge the parent's tree is untouched.
		expect(await readFile(join(root, "app.py"), "utf8")).toBe(APP);
		expect(git(root, "status", "--porcelain")).toBe(statusBefore);
		const merged = await merge([one.rlm_child_id, three.rlm_child_id]);
		expect(merged).toMatchObject({ ok: true, merged: [one.rlm_child_id, three.rlm_child_id] });
		expect(merged.results.map((item) => item.status)).toEqual(["merged", "merged"]);
		const text = await readFile(join(root, "app.py"), "utf8");
		expect(text).toContain("return 'one'");
		expect(text).toContain("return 'three'");
		expect(text).toContain("return 2");
		// Uncommitted: same HEAD and index; the parent's notes are still just untracked.
		expect(git(root, "ls-files", "-s")).toBe(indexBefore);
		expect(git(root, "status", "--porcelain")).toContain(" M app.py");
		expect(git(root, "status", "--porcelain")).toContain("?? three.py");
		expect(await readFile(join(root, "notes.md"), "utf8")).toBe("parent's notes\n");
		// Merged worktrees and branches are gone.
		expect(existsSync(first!.worktree!.path)).toBe(false);
		expect(git(root, "branch", "--list", "ultron/*")).toBe("");
		expect(await listRecords(root)).toEqual([]);
		// Merging again is a no-op report.
		expect((await merge([one.rlm_child_id])).results[0]).toMatchObject({ status: "merged", note: "already merged" });
	});

	test("a conflict stops the merge, reports files and hunks, and leaves the tree as the earlier merges made it", async () => {
		const root = await repository();
		const { spawn, collect, merge } = worktreeHost(root, {
			a: async ({ edit, finish }) => {
				await edit("app.py", "return 2", "return 'a'");
				await finish(passed(["app.py"]));
				return "a";
			},
			b: async ({ edit, finish, cwd }) => {
				await edit("app.py", "return 2", "return 'b'");
				await writeFile(join(cwd, "b.txt"), "b\n");
				await finish(passed(["app.py", "b.txt"]));
				return "b";
			},
			c: async ({ edit, finish }) => {
				await edit("app.py", "return 3", "return 'c'");
				await finish(passed(["app.py"]));
				return "c";
			},
		});
		const handles = [await spawn("a"), await spawn("b"), await spawn("c")];
		const ids = handles.map((handle) => handle.rlm_child_id);
		const results = await collect(ids);
		const report = await merge(ids);
		expect(report.ok).toBe(false);
		expect(report.stopped_at).toBe(ids[1]);
		expect(report.results.map((item) => item.status)).toEqual(["merged", "conflict", "not_attempted"]);
		expect(report.results[1]).toMatchObject({
			name: "b",
			branch: "ultron/session/b",
			files: ["app.py"],
			markers: false,
			conflicts: [{ path: "app.py", hunks: [{ ours: "    return 'a'", theirs: "    return 'b'" }] }],
			message: expect.stringContaining("nothing of b was written"),
		});
		const text = await readFile(join(root, "app.py"), "utf8");
		expect(text).toContain("return 'a'");
		expect(text).not.toContain("<<<<<<<");
		expect(text).not.toContain("return 'c'");
		expect(existsSync(join(root, "b.txt"))).toBe(false);
		// The conflicting child's worktree and branch stay for inspection; the unattempted one's too.
		expect(existsSync(results[1]!.worktree!.path)).toBe(true);
		expect(git(root, "branch", "--list", "ultron/session/b")).toContain("ultron/session/b");
		// Skipping the conflict merges the next child.
		const skipped = await merge([ids[1]!, ids[2]!], { on_conflict: "skip" });
		expect(skipped.results.map((item) => item.status)).toEqual(["conflict", "merged"]);
		expect(await readFile(join(root, "app.py"), "utf8")).toContain("return 'c'");
	});

	test("a completion event says the work waits on its branch", () => {
		const worktree = { branch: "ultron/s/a", commit: "abc", changed_files: ["a.py", "b.py"] };
		expect(
			verdictTag({
				verdict: { status: "passed" },
				check: { outcome: "verified", unobserved: [], unreported: [] },
				worktree,
			}),
		).toBe("[passed, verified; 2 file(s) on ultron/s/a, not merged yet]");
		expect(verdictTag({ verdict: null, unverified: true, worktree })).toBe(
			"[unverified; 2 file(s) on ultron/s/a, not merged yet]",
		);
		expect(verdictTag({ verdict: null, unverified: true, worktree: { ...worktree, commit: null } })).toBe(
			"[unverified]",
		);
	});

	test("failed children are skipped; empty ones clean up at once; merging all worktree children by default", async () => {
		const root = await repository();
		const { spawn, collect, merge } = worktreeHost(root, {
			broken: async ({ edit, finish }) => {
				await edit("app.py", "return 1", "return None");
				await finish({ status: "failed", summary: "Could not make it work", changed_files: ["app.py"] });
				return "failed";
			},
			idle: async ({ finish }) => {
				await finish({ status: "passed", summary: "Nothing needed", evidence: ["app.py:1 already right"] });
				return "nothing to do";
			},
		});
		const broken = await spawn("broken");
		const idle = await spawn("idle");
		const [brokenResult, idleResult] = await collect([broken.rlm_child_id, idle.rlm_child_id]);
		// The failed child's attempt is still committed on its branch, for inspection.
		expect(brokenResult!.worktree?.commit).toMatch(/^[0-9a-f]{40}$/);
		expect(idleResult!.worktree).toMatchObject({ commit: null, changed_files: [], removed: true });
		expect(existsSync(idleResult!.worktree!.path)).toBe(false);
		const report = await merge([]);
		expect(report.results.map((item) => [item.id, item.status])).toEqual([
			[broken.rlm_child_id, "skipped"],
			[idle.rlm_child_id, "empty"],
		]);
		expect(await readFile(join(root, "app.py"), "utf8")).toBe(APP);
		const forced = await merge([broken.rlm_child_id], { include_failed: true });
		expect(forced.results[0]!.status).toBe("merged");
		expect(await readFile(join(root, "app.py"), "utf8")).toContain("return None");
	});

	test("writes into the parent's tree are reported as outside the worktree; absolute parent paths map to the worktree", async () => {
		const root = await repository();
		const { spawn, collect } = worktreeHost(root, {
			stray: async ({ cwd, finish }) => {
				await writeFile(join(root, "stray.txt"), "written in the parent's tree\n");
				await writeFile(join(cwd, "own.txt"), "mine\n");
				// Named by its path in the parent's checkout: the host reads it as the worktree's file.
				await finish(passed([join(root, "own.txt")]));
				return "done";
			},
		});
		const handle = await spawn("stray");
		const [result] = await collect([handle.rlm_child_id]);
		expect(result!.verdict?.changed_files).toEqual(["own.txt"]);
		expect(result!.check).toMatchObject({ outcome: "verified", outside: ["stray.txt"] });
	});

	test("grandchildren branch from their parent's worktree and merge into it; a plain grandchild shares it", async () => {
		const root = await repository();
		let childTree = "";
		const { spawn, collect, merge, fixture } = worktreeHost(root, {
			parent: async ({ cwd, edit, call, finish }) => {
				childTree = cwd;
				await edit("app.py", "return 1", "return 'parent'");
				const grand = await call<{ rlm_child_id: string; worktree: { cwd: string } }>("rlm.spawn", {
					prompt: "grand",
					kwargs: { name: "grand", worktree: true },
				});
				const helper = await call<{ rlm_child_id: string; worktree: null }>("rlm.spawn", {
					prompt: "helper",
					kwargs: { name: "helper" },
				});
				expect(helper.worktree).toBeNull();
				await call("rlm.collect", { selectors: [grand.rlm_child_id, helper.rlm_child_id] });
				const report = await call<{ ok: boolean }>("rlm.merge", { selectors: [grand.rlm_child_id] });
				expect(report.ok).toBe(true);
				await finish(passed(["app.py", "helper.txt"]));
				return "parent done";
			},
			grand: async ({ cwd, edit, finish }) => {
				// It starts from its parent's worktree, uncommitted edit included.
				expect(cwd).not.toBe(childTree);
				expect(await readFile(join(cwd, "app.py"), "utf8")).toContain("return 'parent'");
				await edit("app.py", "return 3", "return 'grand'");
				await finish(passed(["app.py"]));
				return "grand done";
			},
			helper: async ({ cwd, finish }) => {
				expect(cwd).toBe(childTree);
				await writeFile(join(cwd, "helper.txt"), "helped\n");
				await finish(passed(["helper.txt"]));
				return "helper done";
			},
		});
		const handle = await spawn("parent", { depth: 1 });
		const [result] = await collect([handle.rlm_child_id]);
		expect(result!.status).toBe("succeeded");
		expect(result!.worktree?.changed_files).toEqual(["app.py", "helper.txt"]);
		expect((await merge([handle.rlm_child_id])).ok).toBe(true);
		const text = await readFile(join(root, "app.py"), "utf8");
		expect(text).toContain("return 'parent'");
		expect(text).toContain("return 'grand'");
		expect(await readFile(join(root, "helper.txt"), "utf8")).toBe("helped\n");
		// The graph shows each worktree child's branch and how its merge went.
		const status = await fixture.call<{ tasks: Array<{ id: string; worktree?: string }> }>("agents.status", {
			graph: true,
		});
		expect(status.tasks.find((task) => task.id === handle.rlm_child_id)?.worktree).toBe(
			"ultron/session/parent · merged",
		);
	});

	test("a setup command runs in the worktree first; its failure fails the child", async () => {
		const root = await repository();
		const { spawn, collect } = worktreeHost(root, {
			built: async ({ cwd, finish }) => {
				expect(await readFile(join(cwd, "setup.log"), "utf8")).toContain(cwd);
				await finish({ status: "passed", summary: "Checked setup", evidence: ["setup.log:1 present"] });
				return "ok";
			},
			broken: async () => "never runs",
		});
		const good = await spawn("built", {
			worktree_setup: { command: 'pwd > setup.log; echo "$ULTRON_WORKTREE" >> setup.log' },
		});
		const bad = await spawn("broken", { worktree_setup: { command: "echo boom >&2; exit 3" } });
		const [ok, failed] = await collect([good.rlm_child_id, bad.rlm_child_id]);
		expect(ok!.status).toBe("succeeded");
		// Setup output is not the child's work.
		expect(ok!.worktree?.changed_files).toEqual([]);
		expect(failed).toMatchObject({ status: "failed", error: expect.stringContaining("failed (exit 3): boom") });
	});

	test('outside Git worktree=True fails clearly; worktree="auto" falls back to the shared tree', async () => {
		const plain = realpathSync(await mkdtemp(join(tmpdir(), "ultron-worktree-host-plain-")));
		directories.push(plain);
		const { spawn, collect } = worktreeHost(plain, {
			shared: async ({ cwd, finish }) => {
				expect(cwd).toBe(plain);
				await writeFile(join(cwd, "shared.txt"), "x");
				await finish(passed(["shared.txt"]));
				return "ok";
			},
		});
		await expect(spawn("shared")).rejects.toThrow(/worktree=True: worktree=True needs a Git repository/);
		await expect(spawn("shared", { worktree: "yes" })).rejects.toThrow(/must be True, False or "auto"/);
		await expect(spawn("shared", { worktree: false, worktree_setup: { command: "x" } })).rejects.toThrow(
			/worktree_setup needs worktree=True/,
		);
		const auto = await spawn("shared", { worktree: "auto" });
		expect(auto.worktree).toBeNull();
		const [result] = await collect([auto.rlm_child_id]);
		expect(result!.worktree).toBeUndefined();
		expect(result!.check?.outcome).toBe("verified");
	});

	test("workflow nodes run in worktrees with `worktree: true` and merge by their results", async () => {
		const root = await repository();
		const { fixture, merge } = worktreeHost(root, {
			left: async ({ edit, finish }) => {
				await edit("app.py", "return 1", "return 'left'");
				await finish(passed(["app.py"]));
				return "left";
			},
			right: async ({ edit, finish }) => {
				await edit("app.py", "return 3", "return 'right'");
				await finish(passed(["app.py"]));
				return "right";
			},
		});
		const outcome = await fixture.call<Record<string, ChildResult>>("workflows.run", {
			nodes: [
				{ id: "left", definition: "rlm-child@1", input: { prompt: "left" }, worktree: true },
				{ id: "right", definition: "rlm-child@1", input: { prompt: "right" }, worktree: true },
			],
		});
		expect(outcome.left!.worktree?.branch).toBe("ultron/session/left");
		expect(await readFile(join(root, "app.py"), "utf8")).toBe(APP);
		const report = await merge([outcome.left!.worktree!.task, outcome.right!.worktree!.task]);
		expect(report.ok).toBe(true);
		const text = await readFile(join(root, "app.py"), "utf8");
		expect(text).toContain("return 'left'");
		expect(text).toContain("return 'right'");
		await expect(
			fixture.call("workflows.run", {
				nodes: [{ id: "x", definition: "rlm-child@1", input: { prompt: "left" }, worktree: "sometimes" }],
			}),
		).rejects.toThrow(/must be True, False or "auto"/);
	});

	test("closing the session removes finished children's worktrees and keeps their unmerged branches", async () => {
		const root = await repository();
		const { spawn, collect, fixture } = worktreeHost(root, {
			done: async ({ edit, finish }) => {
				await edit("app.py", "return 2", "return 'done'");
				await finish(passed(["app.py"]));
				return "done";
			},
		});
		const handle = await spawn("done");
		const [result] = await collect([handle.rlm_child_id]);
		expect(existsSync(result!.worktree!.path)).toBe(true);
		await fixture.host.close();
		fixtures.splice(fixtures.indexOf(fixture), 1);
		expect(existsSync(result!.worktree!.path)).toBe(false);
		expect(git(root, "show", `ultron/session/done:app.py`)).toContain("return 'done'");
	});

	test("Claude Code children (`ultron claude`) run in their worktree: the process, its server and its verdict", async () => {
		const root = await repository();
		const runs: NativeExternalChildRun[] = [];
		const fixture = hostFixture({
			workspace: root,
			externalChild: async (run) => {
				runs.push(run);
				await writeFile(join(run.cwd!, "claude.txt"), "from claude\n");
				await fixture.host.handle("rlm.finish", passed(["claude.txt"]), {} as never, { lane: run.laneName });
				return { text: "done", turns: 1, toolCalls: 1 };
			},
		});
		fixtures.push(fixture);
		const handle = await fixture.call<{ rlm_child_id: string; worktree: { cwd: string; path: string } }>(
			"rlm.spawn",
			{
				prompt: "write claude.txt",
				kwargs: { name: "claude-child", worktree: true },
			},
		);
		const [item] = (
			await fixture.call<{ results: Array<{ result: ChildResult }> }>("rlm.collect", {
				selectors: [handle.rlm_child_id],
			})
		).results;
		expect(runs[0]!.cwd).toBe(handle.worktree.cwd);
		expect(runs[0]!.env).toMatchObject({ ULTRON_WORKTREE: handle.worktree.path, ULTRON_PARENT_REPO: root });
		expect(runs[0]!.prompt).toContain("[Worktree] You work in your own Git worktree");
		expect(item!.result.check?.outcome).toBe("verified");
		expect(item!.result.worktree?.changed_files).toEqual(["claude.txt"]);
		expect(existsSync(join(root, "claude.txt"))).toBe(false);

		// The `claude -p` process itself starts in the worktree, and its `ultron mcp --child` server is told so.
		const bin = realpathSync(await mkdtemp(join(tmpdir(), "ultron-fake-claude-")));
		directories.push(bin);
		const fake = join(bin, "claude");
		await writeFile(
			fake,
			`#!/bin/sh\npwd > "${bin}/cwd.txt"\necho "$ULTRON_WORKTREE" >> "${bin}/cwd.txt"\nprintf '%s\\n' "$@" > "${bin}/args.txt"\ncat > /dev/null\necho '{"type":"result","result":"ok","num_turns":1}'\n`,
		);
		await chmod(fake, 0o755);
		const controller = new AbortController();
		const result = await runClaudeChild(
			{
				claude: fake,
				self: { command: "ultron", args: [] },
				cwd: root,
				parentSocket: "/tmp/none.sock",
				parentName: "parent",
				model: "sonnet",
				registerChild: () => () => {},
			},
			{
				...runs[0]!,
				signal: controller.signal,
				progress: () => {},
			},
		);
		expect(result.text).toBe("ok");
		expect((await readFile(join(bin, "cwd.txt"), "utf8")).split("\n").slice(0, 2)).toEqual([
			handle.worktree.cwd,
			handle.worktree.path,
		]);
		const args = await readFile(join(bin, "args.txt"), "utf8");
		expect(args).toContain(`"ULTRON_CHILD_CWD":"${handle.worktree.cwd}"`);
		expect(args).toContain(`"ULTRON_WORKTREE":"${handle.worktree.path}"`);
	});
});
