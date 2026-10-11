// Generates crates/delegation/fixtures/worktrees.json from packages/coding-agent/src/ultron/rlm/worktrees.ts: scripted
// scenarios on throwaway Git repositories (create a child worktree, edit in it, commit, merge back with and without
// conflicts, setup links and copies, removal, pruning of dead owners), plus the pure helpers (slug via branch names,
// commitMessage, conflictHunks). Every op's result or error is recorded; temp paths become <root>, commit and tree
// ids are numbered by first appearance (#0, #1, ...), worktree ids become wt#<n>, and pid/host/createdAt are fixed.
// Usage: node crates/delegation/fixtures/generate-worktrees.mjs
import { execFileSync } from "node:child_process";
import { lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const W = await import(join(here, "../../../packages/coding-agent/src/ultron/rlm/worktrees.ts"));

const QUIET = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];
const commitAll = ["add", "-A"];

const scenarios = [
	{ name: "child edits, commits and merges into a clean parent", ops: [
		["init", "repo", { "a.txt": "one\n", "src/b.txt": "b\n", "src/keep.txt": "k\n" }],
		["create", { cwd: "repo", sessionId: "Session-ABC_123", name: "Fix the Parser!" }],
		["read", "$0/a.txt"],
		["write", "$0/a.txt", "one\ntwo\n"], ["write", "$0/new.txt", "fresh\n"], ["rm", "$0/src/b.txt"],
		["dirty", "$0"],
		["commit", 0, { summary: "Teach the parser two", status: "passed", taskId: "task-1" }],
		["git", "$0", ["log", "-1", "--format=%B"]],
		["merge", "repo", 0, "stop"],
		["read", "repo/a.txt"], ["read", "repo/new.txt"], ["read", "repo/src/b.txt"],
		["status", "repo"],
		["merge", "repo", 0, "stop"],
		["remove", 0, true], ["branches", "repo"], ["records", "repo"], ["exists", "$0"],
	]},
	{ name: "dirty parent is snapshotted; an unchanged child is empty", ops: [
		["init", "repo", { "a.txt": "one\n" }],
		["write", "repo/a.txt", "changed\n"], ["write", "repo/untracked.txt", "u\n"],
		["create", { cwd: "repo", sessionId: "s", name: "x" }],
		["read", "$0/a.txt"], ["read", "$0/untracked.txt"],
		["commit", 0, { summary: "", status: "failed" }],
		["merge", "repo", 0, "stop"],
		["status", "repo"],
	]},
	{ name: "branch names are unique and slugged; a subdirectory cwd maps into the worktree", ops: [
		["init", "repo", { "src/deep/f.txt": "f\n" }],
		["create", { cwd: "repo", sessionId: "sess", name: "same" }],
		["create", { cwd: "repo", sessionId: "sess", name: "same" }],
		["create", { cwd: "repo", sessionId: "sess", name: "..." }],
		["create", { cwd: "repo", sessionId: "Ünïcode Session", name: "Ünïcode -- Name..with..dots" }],
		["create", { cwd: "repo/src/deep", sessionId: "sess", name: "a-very-long-name-that-goes-on-and-on-and-on-and-on-forever" }],
		["read", "$4/src/deep/f.txt"],
		["branches", "repo"],
		["notes", 4],
	]},
	{ name: "conflicts stop, skip writing, or write markers", ops: [
		["init", "repo", { "a.txt": "line 1\nshared\nline 3\n", "b.txt": "b\n" }],
		["create", { cwd: "repo", sessionId: "s", name: "c" }],
		["write", "$0/a.txt", "line 1\nchild\nline 3\n"], ["write", "$0/b.txt", "b child\n"],
		["commit", 0, { summary: "child edit", status: "passed" }],
		["write", "repo/a.txt", "line 1\nparent\nline 3\n"],
		["merge", "repo", 0, "stop"],
		["read", "repo/a.txt"], ["read", "repo/b.txt"],
		["merge", "repo", 0, "markers"],
		["read", "repo/a.txt"], ["read", "repo/b.txt"],
	]},
	{ name: "a child deleting a whole directory and a clean merge after a parent edit elsewhere", ops: [
		["init", "repo", { "one/two/f.txt": "f\n", "top.txt": "t\n", "other.txt": "o\n" }],
		["create", { cwd: "repo", sessionId: "s", name: "d" }],
		["rm", "$0/one/two/f.txt"], ["write", "$0/top.txt", "t2\n"],
		["commit", 0, { summary: "drop one", status: "passed" }],
		["write", "repo/other.txt", "parent edit\n"],
		["merge", "repo", 0, "stop"],
		["exists", "repo/one"], ["read", "repo/top.txt"], ["read", "repo/other.txt"], ["status", "repo"],
	]},
	{ name: "commit limited to paths", ops: [
		["init", "repo", { "a.txt": "a\n" }],
		["create", { cwd: "repo", sessionId: "s", name: "p" }],
		["write", "$0/one.txt", "1\n"], ["write", "$0/two.txt", "2\n"],
		["commitPaths", 0, ["one.txt"]],
		["dirty", "$0"],
	]},
	{ name: "setup links dependency trees and copies env files", ops: [
		["init", "repo", { ".gitignore": "node_modules/\n.env\n.env.*\n.venv/\nbig.env\n", "a.txt": "a\n", "pkgs/w/index.js": "w\n" }],
		["write", "repo/node_modules/left-pad/index.js", "lp\n"],
		["write", "repo/node_modules/@scope/x/index.js", "x\n"],
		["symlink", "../pkgs/w", "repo/node_modules/w"],
		["write", "repo/.env", "K=1\n"], ["write", "repo/.env.local", "L=2\n"],
		["write", "repo/.venv/bin/python", "py\n"],
		["create", { cwd: "repo", sessionId: "s", name: "setup" }],
		["ls", "$0/node_modules"], ["ls", "$0/node_modules/@scope"], ["ls", "$0/.venv"],
		["read", "$0/.env"], ["read", "$0/.env.local"],
		["dirty", "$0"],
		["commit", 0, { summary: "nothing", status: "passed" }],
		["remove", 0, true],
		["ls", "repo/node_modules"],
		["create", { cwd: "repo", sessionId: "s", name: "explicit", setup: { link: [], copy: [".env"], command: "  echo hi  ", timeoutMs: 5000 } }],
		["ls", "$1"],
		["setupCommand", 1],
		["create", { cwd: "repo", sessionId: "s", name: "bad", setup: { link: ["missing"] } }],
		["create", { cwd: "repo", sessionId: "s", name: "bad", setup: { copy: ["/etc/passwd"] } }],
		["create", { cwd: "repo", sessionId: "s", name: "bad", setup: { link: ["a/../b"] } }],
		["create", { cwd: "repo", sessionId: "s", name: "bad", setup: { copy: ["node_modules"] } }],
		["create", { cwd: "repo", sessionId: "s", name: "failing", setup: { link: [], copy: [], command: "echo out; echo err >&2; exit 3" } }],
		["setupCommand", 2],
		["branches", "repo"], ["records", "repo"],
	]},
	{ name: "not a repository, and a repository without commits", ops: [
		["mkdir", "plain"],
		["create", { cwd: "plain", sessionId: "s", name: "x" }],
		["git", "", ["init", "-q", "-b", "main", "empty"]],
		["create", { cwd: "empty", sessionId: "s", name: "x" }],
		["records", "plain"],
	]},
	{ name: "pruning dead owners keeps branches with work", ops: [
		["init", "repo", { "a.txt": "a\n" }],
		["create", { cwd: "repo", sessionId: "s", name: "worked" }],
		["create", { cwd: "repo", sessionId: "s", name: "idle" }],
		["create", { cwd: "repo", sessionId: "s", name: "alive" }],
		["write", "$0/a.txt", "uncommitted\n"],
		["deadOwner", 0], ["deadOwner", 1],
		["prune", "repo", {}],
		["branches", "repo"], ["records", "repo"],
		["git", "repo", ["log", "-1", "--format=%B", "ultron/s/worked"]],
		["prune", "repo", { force: true, deleteBranches: true }],
		["branches", "repo"], ["records", "repo"],
	]},
];

const pure = {
	commitMessage: [
		["", "kid", undefined, "failed"],
		["  short summary  ", "kid", "task-9", "passed"],
		["first line\nsecond line\n\nthird", "kid", "t", "passed"],
		[`${"x".repeat(80)}\nmore`, "kid", "t", "passed"],
		[`${"é".repeat(72)}`, "kid", undefined, "blocked"],
		[`a${"😀".repeat(40)}`, "kid", undefined, "passed"],
	].map((args) => ({ args: args.map((a) => a ?? null), result: W.commitMessage(...args) })),
	conflictHunks: [
		"no conflicts\n",
		"a\n<<<<<<< ours\nmine\n=======\ntheirs\n>>>>>>> b\nz\n",
		"<<<<<<< o\nx\n||||||| base\nbase\n=======\ny\n>>>>>>> t\n<<<<<<< o\nunterminated\n",
		Array.from({ length: 7 }, (_, i) => `<<<<<<< o\n${i}\n=======\nt${i}\n>>>>>>> t`).join("\n"),
		`<<<<<<< o\n${"o".repeat(500)}\n=======\n${"t".repeat(399)}\n>>>>>>> t\n`,
	].map((text) => ({ text, result: W.conflictHunks(text) })),
	keepWorktrees: [undefined, "", "0", "false", " No ", "off", "1", "yes", "anything"].map((value) => ({
		value: value ?? null,
		result: W.keepWorktrees(value === undefined ? {} : { ULTRON_KEEP_WORKTREES: value }),
	})),
};

function sh(cwd, args) {
	return execFileSync("git", args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

const results = [];
for (const scenario of scenarios) {
	const root = realpathSync(mkdtempSync(join(tmpdir(), "ultron-wt-fixture-")));
	const records = [];
	const commits = [];
	const resolvePath = (p) => p.replace(/^\$(\d+)/, (_, n) => records[Number(n)].path).replace(/^(?!\/)/, `${root}/`);
	const steps = [];
	for (const op of scenario.ops) {
		const [kind, ...args] = op;
		let result;
		try {
			if (kind === "init") {
				const dir = join(root, args[0]);
				mkdirSync(dir, { recursive: true });
				sh(dir, ["init", "-q", "-b", "main"]);
				sh(dir, ["config", "user.name", "Fixture"]);
				sh(dir, ["config", "user.email", "fixture@example.com"]);
				for (const [path, text] of Object.entries(args[1])) {
					mkdirSync(dirname(join(dir, path)), { recursive: true });
					writeFileSync(join(dir, path), text);
				}
				sh(dir, [...QUIET, ...commitAll]);
				// Unique per scenario: identical initial commits made within one second would share an id.
				sh(dir, [...QUIET, "commit", "-q", "-m", `initial: ${scenario.name}`]);
				result = null;
			} else if (kind === "write") {
				const path = resolvePath(args[0]);
				mkdirSync(dirname(path), { recursive: true });
				writeFileSync(path, args[1]);
				result = null;
			} else if (kind === "symlink") {
				const path = resolvePath(args[1]);
				mkdirSync(dirname(path), { recursive: true });
				execFileSync("ln", ["-s", args[0], path]);
				result = null;
			} else if (kind === "mkdir") {
				mkdirSync(resolvePath(args[0]), { recursive: true });
				result = null;
			} else if (kind === "rm") {
				rmSync(resolvePath(args[0]));
				result = null;
			} else if (kind === "read") {
				try { result = readFileSync(resolvePath(args[0]), "utf8"); } catch { result = null; }
			} else if (kind === "exists") {
				try { lstatSync(resolvePath(args[0])); result = true; } catch { result = false; }
			} else if (kind === "ls") {
				const dir = resolvePath(args[0]);
				result = readdirSync(dir).sort().map((name) => {
					const info = lstatSync(join(dir, name));
					return info.isSymbolicLink() ? { name, link: readlinkSync(join(dir, name)) } : { name, dir: info.isDirectory() };
				});
			} else if (kind === "create") {
				const o = args[0];
				const record = await W.createChildWorktree({ cwd: resolvePath(o.cwd), sessionId: o.sessionId, name: o.name, ...(o.setup ? { setup: o.setup } : {}) });
				records.push(record);
				result = record;
			} else if (kind === "dirty") {
				result = await W.listDirty(resolvePath(args[0]));
			} else if (kind === "commit") {
				const record = records[args[0]];
				const o = args[1];
				const outcome = await W.commitChildWorktree(record, { message: W.commitMessage(o.summary, record.name, o.taskId, o.status) });
				commits[args[0]] = outcome;
				result = outcome;
			} else if (kind === "commitPaths") {
				const record = records[args[0]];
				const outcome = await W.commitChildWorktree(record, { message: "limited", paths: args[1] });
				commits[args[0]] = outcome;
				result = outcome;
			} else if (kind === "merge") {
				const record = records[args[1]];
				result = await W.mergeChildWorktree(resolvePath(args[0]), { name: record.name, branch: record.branch, base: record.base, commit: commits[args[1]].commit },
					{ onConflict: args[2] });
			} else if (kind === "remove") {
				await W.removeChildWorktree(records[args[0]], { deleteBranch: args[1] });
				result = null;
			} else if (kind === "branches") {
				result = sh(resolvePath(args[0]), ["for-each-ref", "--format=%(refname:short)", "refs/heads"]).split("\n").filter(Boolean);
			} else if (kind === "records") {
				result = await W.listRecords(resolvePath(args[0]));
			} else if (kind === "status") {
				result = sh(resolvePath(args[0]), ["status", "--porcelain=v1", "--untracked-files=all"]);
			} else if (kind === "git") {
				result = sh(args[0] === "" ? root : resolvePath(args[0]), args[1]);
			} else if (kind === "notes") {
				const record = records[args[0]];
				result = { env: W.worktreeEnv(record), brief: W.worktreeBriefNote(record) };
			} else if (kind === "setupCommand") {
				await W.runSetupCommand(records[args[0]]);
				result = null;
			} else if (kind === "deadOwner") {
				const record = records[args[0]];
				const path = join(record.commonDir, W.WORKTREES_DIR, `${record.id}.json`);
				const saved = JSON.parse(readFileSync(path, "utf8"));
				saved.owner.pid = 2147483646;
				writeFileSync(path, `${JSON.stringify(saved, null, "\t")}\n`);
				result = null;
			} else if (kind === "prune") {
				result = await W.pruneWorktrees(resolvePath(args[0]), args[1]);
			} else throw new Error(`unknown op ${kind}`);
			steps.push({ op, result: result === undefined ? null : JSON.parse(JSON.stringify(result)) });
		} catch (error) {
			steps.push({ op, error: error.message, fallback: error.fallback ?? null });
		}
	}
	rmSync(root, { recursive: true, force: true });
	results.push({ name: scenario.name, steps, ids: records.map((r) => r.id), root });
}

// Normalize: temp roots, worktree ids, hashes, pid/host/createdAt.
let text = JSON.stringify({ scenarios: results, pure }, null, "\t");
for (const scenario of results) {
	text = text.split(scenario.root).join("<root>");
	scenario.ids.forEach((id, n) => { text = text.split(id).join(`wt#${n}`); });
}
const hashes = new Map();
text = text.replace(/\b[0-9a-f]{40}\b/g, (h) => { if (!hashes.has(h)) hashes.set(h, `#${hashes.size}`); return hashes.get(h); });
const parsed = JSON.parse(text);
for (const scenario of parsed.scenarios) {
	delete scenario.root;
	delete scenario.ids;
	const fix = (value) => {
		if (Array.isArray(value)) return value.forEach(fix);
		if (value && typeof value === "object") {
			if (value.owner && typeof value.owner === "object") value.owner = { pid: 0, host: "<host>" };
			if (typeof value.createdAt === "number") value.createdAt = 0;
			Object.values(value).forEach(fix);
		}
	};
	fix(scenario.steps);
}
writeFileSync(join(here, "worktrees.json"), `${JSON.stringify(parsed, null, "\t")}\n`);
